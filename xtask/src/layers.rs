// SPDX-License-Identifier: MPL-2.0
//! `check-layers` and `check-versions`: the crate DAG of `docs/02-architecture.md` § 1, the runtime
//! exclusions of `docs/06-testing-and-ci.md` § 7 and the coupling groups of `docs/03` § 2.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::Graph;

/// Where a workspace crate sits in the architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Layer 0a: core, ecs, app, macros.
    Foundation,
    /// Layer 0b: platform.
    Platform,
    /// Layer 1: scene, asset, input.
    World,
    /// Layer 2: render, physics, audio, animation, ui, terrain, vfx.
    Systems,
    /// Layer 3: framework, agent, steam.
    Gameplay,
    /// Layer 4: the umbrella crate and the engine dylib.
    Umbrella,
    /// Samples (games built on the engine): may use runtime crates and other samples; nothing else may
    /// depend on them.
    Sample,
    /// Layer 5: desktop-only importers and cookers.
    Pipeline,
    /// Layer 6: CLI and editor.
    Tools,
    /// Dev-dependency only (`regia_test_utils`, `regia_net_soak`).
    TestOnly,
    /// Repository automation (`xtask`); nothing may depend on it.
    Automation,
}

impl Layer {
    /// Layers 0–4 end up in shipped games.
    pub fn is_runtime(self) -> bool {
        self <= Layer::Umbrella
    }

    /// Crates whose resolved tree must stay free of editor and importer code: the runtime layers and
    /// samples (which are games).
    pub fn ships_in_games(self) -> bool {
        self.is_runtime() || self == Layer::Sample
    }
}

/// The layer table. Every workspace crate must be listed; new crates are added here with the spec change.
pub fn layer_of(name: &str) -> Option<Layer> {
    Some(match name {
        "regia_core" | "regia_ecs" | "regia_app" | "regia_macros" | "regia_detsim" | "regia_world_geom" => {
            Layer::Foundation
        }
        "regia_rules" => Layer::Foundation,
        // ADR-8877: the deterministic navigation, out of regia_nav (which re-exports it as `det`).
        "regia_nav_det" => Layer::Foundation,
        "regia_platform" | "regia_dlss" | "regia_xess" => Layer::Platform,
        "regia_scene" | "regia_asset" | "regia_input" => Layer::World,
        "regia_render" | "regia_physics" | "regia_audio" | "regia_animation" | "regia_ui"
        | "regia_terrain" | "regia_pick" | "regia_vfx" | "regia_nav" | "regia_matgraph" | "regia_pt"
        | "regia_2d" | "regia_net" | "regia_xr" | "regia_world" | "regia_fluid" | "regia_viewer"
        | "regia_looks" | "regia_viewer_view" | "regia_viewer_pack" | "regia_gen" => Layer::Systems,
        "regia_framework" | "regia_agent" | "regia_steam" | "regia_ai" | "regia_crash" | "regia_quick"
        | "regia_telemetry" | "regia_procgen" | "regia_media" | "regia_script_lua" | "regia_configurator"
        | "regia_web" => Layer::Gameplay,
        "regia" | "regia_dylib" => Layer::Umbrella,
        "regia_asset_pipeline" | "regia_templates" | "regia_release_chunks" | "render_lab" => Layer::Pipeline,
        "regia_cli" | "regia_editor" => Layer::Tools,
        "regia_test_utils" | "regia_net_soak" => Layer::TestOnly,
        "xtask" => Layer::Automation,
        "hello_window"
        | "hello_window_android"
        | "dedicated_server"
        | "sim_bridge"
        | "sim_bridge_android"
        | "input_lab"
        | "ui_gallery"
        | "ui_gallery_android"
        | "audio_lab"
        | "anim_lab"
        | "crowd_lab"
        | "terrain_lab"
        | "procgen_common"
        | "procgen_roads"
        | "procgen_biomes"
        | "procgen_open_world"
        | "lookdev"
        | "showcase"
        | "boot_probe"
        | "vfx_lab"
        | "vfx_lab_android"
        | "sky_lab"
        | "hot_fixture"
        | "ext_fixture"
        | "third_person"
        | "stream_world"
        | "arena"
        | "arena_android"
        | "net_game"
        | "nav_world_lab"
        | "physics_lab"
        | "mm_locomotion"
        | "product_viewer"
        | "product_viewer_android"
        | "vehicle_lab"
        | "destruction_lab"
        | "lightmap_device"
        | "lightmap_device_android"
        | "flight_lab"
        | "bike_lab"
        | "hud_draw2d" => Layer::Sample,
        _ => return None,
    })
}

/// `hot_fixture`'s `hot-reload` feature names the editor so a game rebuild resolves the editor's
/// features and does not relink `regia_dylib` (specs/20 § 1). The fixture source does not
/// reference it. The facade feature on `regia` is the project-template form of the same anchor.
const FEATURE_ANCHOR: &[(&str, &str)] = &[("hot_fixture", "regia_editor"), ("ext_fixture", "regia_editor")];

/// Dependencies between crates of the same layer that 02 § 1 allows explicitly.
const SAME_LAYER_ALLOWED: &[(&str, &str)] = &[
    ("regia_ecs", "regia_core"),
    ("regia_app", "regia_ecs"),
    ("regia_app", "regia_core"),
    // The world-geometry crate uses the lockstep math kit (both 0a, both on the lockstep closure; ADR-8012 A-5,
    // docs/03; first edge: the meshlayers cook, ADR-8112).
    ("regia_world_geom", "regia_detsim"),
    ("regia_scene", "regia_asset"),
    ("regia_ui", "regia_render"),
    ("regia_terrain", "regia_render"),
    ("regia_vfx", "regia_render"),
    // Render-side picking casts rays against mesh renderers and the terrain heightfield with its holes.
    ("regia_pick", "regia_render"),
    ("regia_pick", "regia_terrain"),
    // The 2D toolkit's sprite pass is a `WindowOverlay` layer (specs/22 § T2.10).
    ("regia_2d", "regia_render"),
    // Material graphs compile to the forward pass's surface hook (`regia_render::surface_graph`).
    ("regia_matgraph", "regia_render"),
    // The reference path tracer reads the renderer's extracted frame and the terrain map (specs/27 § 1.1).
    ("regia_pt", "regia_render"),
    ("regia_pt", "regia_terrain"),
    ("regia_dylib", "regia"),
    // The agent server exposes framework tools (scripts, cameras, settings, sim bridge) — ADR-018.
    ("regia_agent", "regia_framework"),
    // The agent server reports the quick draw layer (`quick.draw_list`, the `QuickDraw` predicate).
    ("regia_agent", "regia_quick"),
    // Telemetry reads the framework's frame sampler (shared with the feedback ring) and settings.
    ("regia_telemetry", "regia_framework"),
    // The agent server reports the last telemetry summary (`telemetry.get_last_summary`).
    ("regia_agent", "regia_telemetry"),
    // The agent server exposes the smart-object surface (list / claim / release) over regia_ai's
    // components and helpers (`book/agents/smart-objects.md`).
    ("regia_agent", "regia_ai"),
    // Smart objects v2 match slots by gameplay tags (`regia_framework::tags`, spec 39 § 2).
    ("regia_ai", "regia_framework"),
    // The agent server exposes procedural generators and graphs (`procgen.*`, specs/62 § 9, ADR-1596).
    ("regia_agent", "regia_procgen"),
    // Steam is an optional crate (the `steam` feature of the facade): it registers `steam.status` on the
    // agent's `AgentExtension` itself, so the agent server does not depend on the Steam SDK crate for
    // every game (batch v020, Steam core).
    ("regia_steam", "regia_agent"),
    // XR renders eye views through the renderer (spec 58 § 3.1, the one system-to-system edge).
    ("regia_xr", "regia_render"),
    // Voice chat's relay and client transport live in regia_net behind its `voice` feature; they use
    // the transport trait and the routing table of regia_audio (specs/59 § 3, ADR-1911).
    ("regia_net", "regia_audio"),
    // Video playback is a layer-3 crate (ADR-1593 § 6, ADR-1670); the agent server exposes `video.*`.
    ("regia_agent", "regia_media"),
    // The Steam cloud backends implement the framework's save store (`regia_framework::save_flow`,
    // spec 91 § 2.2, ADR-1691).
    ("regia_steam", "regia_framework"),
    // Lua mods are an optional crate (the `lua` feature of the facade): it registers `lua.*` on the agent's
    // `AgentExtension` itself, so the agent server never links the Lua C runtime (ADR-1751).
    ("regia_script_lua", "regia_agent"),
    // Lua mod state is one section of a save file (`regia_framework::save_game::SaveSections`, ADR-1943).
    ("regia_script_lua", "regia_framework"),
    // The particle liquid's `draw` feature publishes its surface and whitewater as `MeshRenderer`s (ADR-2441).
    ("regia_fluid", "regia_render"),
    // Boot movies from `Regia.toml` play through the media player (`regia_framework::boot_movies`,
    // the framework's optional `media` feature, specs/50 § 2.7, ADR-2410).
    ("regia_framework", "regia_media"),
    // The agent server validates product viewer contracts (`configurator.contract_check`, ADR-2480, ADR-2535).
    ("regia_agent", "regia_configurator"),
    // A render job's `product` block draws through the web player's core (ADR-2757).
    ("regia_agent", "regia_web"),
    // The web player holds a configurator per open product (ADR-2541).
    ("regia_web", "regia_configurator"),
    // The product viewer's cook refuses recipes whose view_range the drag view would not accept (ADR-2540).
    ("regia_viewer_pack", "regia_viewer_view"),
    ("regia_viewer_pack", "regia_looks"),
    // The product looks plug into the viewer's shader hooks and material blocks (ADR-2538).
    ("regia_looks", "regia_viewer"),
    // The web player answers picks through the configurator's sold-combination checks (ADR-2541).
    ("regia_web", "regia_configurator"),
    // The particle liquid's `physics` feature turns rigid bodies into moving solids and applies the
    // liquid's pressure impulses back through `ExternalImpulse` (ADR-2577).
    ("regia_fluid", "regia_physics"),
    // The lockstep world geometry calls the lockstep kit's libm trig and `det_*` min/max/clamp (ADR-8166).
    ("regia_world_geom", "regia_detsim"),
    // ECS-free match rules use the lockstep tick constants (ADR-8239).
    ("regia_rules", "regia_detsim"),
    // The deterministic navigation takes the lockstep kit's math, `det_*` extrema and golden harness, and the
    // tunnel layer reads the world-geometry compat layer (ADR-8877; it was regia_nav's `det` module).
    ("regia_nav_det", "regia_detsim"),
    ("regia_nav_det", "regia_world_geom"),
    // The agent server reports the report outbox's ledger (`report.outbox_status`, ADR-8402).
    ("regia_agent", "regia_crash"),
];

/// External crates that must never be compiled into a runtime crate (06 § 7), plus tokio/rmcp (no async
/// runtime in shipped games; the agent server is synchronous, ADR-014). Name or name prefix.
const RUNTIME_FORBIDDEN_EXACT: &[&str] = &[
    "gltf",
    "image",
    "image-compare",
    "intel_tex_2",
    "meshopt",
    "notify",
    "tiled",
    "texture_packer",
    "tokio",
    "rmcp",
    // FBX import's C parser: pipeline-only FFI, never in a runtime crate or a shipped game (ADR-1602).
    "ufbx",
    // Ogg Vorbis encoder for the audio cook: pipeline-only C bindings (ADR-3230).
    "vorbis_rs",
    "aotuv_lancer_vorbis_sys",
    "ogg_next_sys",
    // USD import's reader: pipeline-only, never in a runtime crate or a shipped game (ADR-3235).
    "openusd",
];
const RUNTIME_FORBIDDEN_PREFIX: &[&str] = &["egui", "epaint", "transform-gizmo"];

/// External crates only `regia_editor` may use directly (04 § 5, 05 M3). Other crates may still reach
/// them indirectly (wgpu loads Vulkan through `libloading`).
const EDITOR_ONLY_DIRECT: &[&str] = &["libloading", "rfd"];

/// The one allowed exception: `regia_asset`'s desktop-only `dev-assets` feature uses `notify`.
/// `render_lab` is a desktop-only developer tool (not a shipped game); it may use `regia_test_utils`.
/// `showcase` is footage tooling that is never shipped inside a game; it decodes glTF textures with
/// `image` (ADR-1439). No other sample may use `image`.
const ALLOWED_RUNTIME_EDGES: &[(&str, &str)] = &[
    ("regia_asset", "notify"),
    ("render_lab", "regia_test_utils"),
    ("showcase", "image"),
    // The product viewer app's option panel is egui on the viewer's own device (spec 42 § 9, ADR-2533 (c), ADR-2580).
    // It links neither `regia_ui` nor `regia_render`; games still may not reach egui.
    ("product_viewer", "egui"),
    ("product_viewer", "egui-wgpu"),
];

/// Crates that must resolve to a single version in the runtime graph (03 § 2 coupling groups).
pub const COUPLED: &[&str] = &[
    "glam",
    "bevy_ecs",
    "bevy_reflect",
    "bevy_tasks",
    "rapier3d",
    "kira",
    "wgpu",
    "naga",
    "naga_oil",
    "winit",
];

fn runtime_forbidden(name: &str) -> bool {
    RUNTIME_FORBIDDEN_EXACT.contains(&name) || RUNTIME_FORBIDDEN_PREFIX.iter().any(|p| name.starts_with(p))
}

fn skip_allowed(graph: &Graph) -> impl Fn(usize, usize) -> bool + '_ {
    move |from, to| {
        let (f, t) = (&graph.packages[from].name, &graph.packages[to].name);
        ALLOWED_RUNTIME_EDGES.iter().any(|(a, b)| a == f && b == t)
    }
}

/// Names of the runtime workspace crates in a declared graph.
pub fn runtime_members(declared: &Graph) -> Vec<String> {
    declared
        .packages
        .iter()
        .filter(|p| p.workspace && layer_of(&p.name).is_some_and(Layer::ships_in_games))
        .map(|p| p.name.clone())
        .collect()
}

/// Direct rules on declared dependencies: layer table, DAG direction, test-utils and editor-only crates.
pub fn check_direct(declared: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    for (idx, pkg) in declared.packages.iter().enumerate().filter(|(_, p)| p.workspace) {
        let Some(layer) = layer_of(&pkg.name) else {
            errors.push(format!(
                "{} is not in the layer table (xtask/src/layers.rs, docs/02-architecture.md § 1)",
                pkg.name
            ));
            continue;
        };
        for edge in &declared.edges[idx] {
            let dep = &declared.packages[edge.to];
            if !dep.workspace {
                if !edge.dev_only
                    && pkg.name != "regia_editor"
                    && EDITOR_ONLY_DIRECT.contains(&dep.name.as_str())
                    && !(pkg.name == "regia_xess" && dep.name == "libloading")
                // ADR-4656: external XeSS DLL only.
                {
                    errors.push(format!(
                        "{} uses {} directly; only regia_editor may",
                        pkg.name, dep.name
                    ));
                }
                continue;
            }
            let Some(dep_layer) = layer_of(&dep.name) else {
                continue;
            };
            if dep_layer == Layer::Automation {
                errors.push(format!("{} depends on xtask", pkg.name));
                continue;
            }
            if edge.dev_only || layer == Layer::TestOnly || layer == Layer::Automation {
                continue;
            }
            if dep_layer == Layer::Sample && layer == Layer::Sample {
                continue;
            }
            if dep_layer == Layer::TestOnly {
                // render_lab is a desktop-only developer tool, not a shipped game.
                if pkg.name != "render_lab" {
                    errors.push(format!(
                        "{} uses {} outside [dev-dependencies]",
                        pkg.name, dep.name
                    ));
                }
            } else if dep_layer == Layer::Sample {
                errors.push(format!("{} must not depend on the sample {}", pkg.name, dep.name));
            } else if (dep_layer > layer && !FEATURE_ANCHOR.contains(&(pkg.name.as_str(), dep.name.as_str())))
                || (dep_layer == layer
                    && !SAME_LAYER_ALLOWED.contains(&(pkg.name.as_str(), dep.name.as_str())))
            {
                errors.push(format!(
                    "{} ({layer:?}) must not depend on {} ({dep_layer:?})",
                    pkg.name, dep.name
                ));
            }
        }
    }
    errors
}

/// Offline-only workspace crates: they sit in a runtime layer (they read the renderer's frame) but a
/// shipped game never links them. The reference path tracer is one (specs/27 § 1.1 and § 6: "never
/// shipped in a game binary"); a runtime crate reaches it only through an opt-in feature
/// (`regia_agent`'s `reference-trace`), which the default tree checked here leaves off.
pub const OFFLINE_ONLY: &[&str] = &["regia_pt"];

/// Shipping-layer crates allowed to link an [`OFFLINE_ONLY`] crate by default, with the reason:
/// `lookdev` holds the offline plate tools (`pt_plates`, specs/27 § 5), and no game depends on it.
pub const OFFLINE_ONLY_ROOTS: &[(&str, &str)] = &[("lookdev", "regia_pt")];

/// Runtime crates that may declare an [`OFFLINE_ONLY`] crate, but only as an *optional* dependency
/// behind a tool-only feature: the agent server's `reference-trace` (and `denoise`, which turns it
/// on) serves `seeing.reference_trace` in desktop tool hosts (specs/27 § 5). No shipping crate may
/// turn such a feature on, and the host's default features must not ([`check_offline_only`]).
pub const OFFLINE_ONLY_OPT_IN: &[(&str, &str)] = &[("regia_agent", "regia_pt")];

/// The features of `host` that turn on its optional dependency `dep`, directly or through other
/// features of `host` (`dep:<dep>`, `<dep>/<f>`, `<dep>?/<f>`, or the implicit feature `<dep>`).
fn features_enabling(declared: &Graph, host: usize, dep: &str) -> BTreeSet<String> {
    let Some(table) = declared.features.get(&host) else {
        return BTreeSet::new();
    };
    let names_dep = |value: &str| {
        value == dep
            || value.strip_prefix("dep:") == Some(dep)
            || value
                .split_once('/')
                .is_some_and(|(name, _)| name.trim_end_matches('?') == dep)
    };
    let mut enabling: BTreeSet<String> = table
        .iter()
        .filter(|(_, values)| values.iter().any(|v| names_dep(v)))
        .map(|(name, _)| name.clone())
        .collect();
    // Features that turn on an enabling feature enable it too (to a fixed point).
    loop {
        let more: Vec<String> = table
            .iter()
            .filter(|(name, values)| !enabling.contains(*name) && values.iter().any(|v| enabling.contains(v)))
            .map(|(name, _)| name.clone())
            .collect();
        if more.is_empty() {
            return enabling;
        }
        enabling.extend(more);
    }
}

/// specs/27 § 1.1 and § 6 on the *declared* graph, every feature included: the reference path
/// tracer (and any other [`OFFLINE_ONLY`] crate) is never a dependency of a runtime crate or a
/// game sample. Allowed: an [`OFFLINE_ONLY_ROOTS`] crate (lookdev's offline `pt_plates`), dev-only
/// edges (tests), the tool layers (editor, CLI, pipeline), and an [`OFFLINE_ONLY_OPT_IN`] host's
/// optional edge, provided no shipping crate and none of the host's default features turn it on.
/// [`check_runtime_tree`] covers the resolved default trees; this catches what only a feature
/// would pull in.
pub fn check_offline_only(declared: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    let shipping = |name: &str| layer_of(name).is_some_and(Layer::ships_in_games);
    let root = |name: &str, dep: &str| OFFLINE_ONLY_ROOTS.contains(&(name, dep));
    for (idx, pkg) in declared.packages.iter().enumerate() {
        if !pkg.workspace || !shipping(&pkg.name) {
            continue;
        }
        for edge in declared.edges[idx].iter().filter(|e| !e.dev_only) {
            let dep = &declared.packages[edge.to];
            if !dep.workspace || !OFFLINE_ONLY.contains(&dep.name.as_str()) || root(&pkg.name, &dep.name) {
                continue;
            }
            let opt_in = OFFLINE_ONLY_OPT_IN.contains(&(pkg.name.as_str(), dep.name.as_str()));
            if opt_in && declared.optional.contains(&(idx, edge.to)) {
                continue;
            }
            errors.push(format!(
                "runtime crate {} declares a dependency on the offline-only {} (specs/27 § 1.1: never a \
                 dependency of a runtime crate){}",
                pkg.name,
                dep.name,
                if opt_in {
                    "; it must stay optional, behind a tool-only feature"
                } else {
                    ""
                }
            ));
        }
    }
    for &(host_name, dep) in OFFLINE_ONLY_OPT_IN {
        let Some(host) = declared
            .packages
            .iter()
            .position(|p| p.workspace && p.name == host_name)
        else {
            continue;
        };
        let enabling = features_enabling(declared, host, dep);
        if enabling.contains("default") {
            errors.push(format!(
                "{host_name}'s default features turn on the offline-only {dep} (specs/27 § 1.1)"
            ));
        }
        for (idx, pkg) in declared.packages.iter().enumerate() {
            if idx == host || !pkg.workspace || !shipping(&pkg.name) || root(&pkg.name, dep) {
                continue;
            }
            for reference in declared.feature_refs.get(&idx).into_iter().flatten() {
                let Some((name, feature)) = reference.split_once('/') else {
                    continue;
                };
                if name.trim_end_matches('?') == host_name && enabling.contains(feature) {
                    errors.push(format!(
                        "runtime crate {} turns on {host_name}/{feature}, which links the offline-only {dep} \
                         (specs/27 § 1.1: only desktop tools may)",
                        pkg.name
                    ));
                }
            }
        }
    }
    errors
}

/// Transitive rule on one runtime crate's resolved tree (root = package 0): nothing from the
/// desktop-only layers, the editor/importer crate list or the offline-only crates is compiled in.
pub fn check_runtime_tree(tree: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    let root = tree.packages[0].name.as_str();
    for (reached, path) in tree.reachable(0, skip_allowed(tree)) {
        let dep = &tree.packages[reached];
        let offline = dep.workspace
            && OFFLINE_ONLY.contains(&dep.name.as_str())
            && !OFFLINE_ONLY_ROOTS.contains(&(root, dep.name.as_str()));
        let bad_member =
            offline || (dep.workspace && layer_of(&dep.name).is_some_and(|l| !l.ships_in_games()));
        if bad_member || (!dep.workspace && runtime_forbidden(&dep.name)) {
            errors.push(format!(
                "runtime crate {} reaches {}: {}",
                tree.packages[0].name,
                dep.name,
                tree.path_string(&path)
            ));
        }
    }
    errors
}

/// Crates a dedicated-server build never compiles (spec 76 § 2.1): the window, renderer and audio
/// stacks and the crates built on them.
pub const SERVER_FORBIDDEN: &[&str] = &[
    "regia_render",
    "regia_audio",
    "regia_ui",
    "regia_vfx",
    "regia_2d",
    "regia_telemetry",
    "winit",
    "wgpu",
    "kira",
    "cpal",
    "gilrs",
    "arboard",
];

/// Transitive rule on the server build's resolved tree (`Graph::server_tree`).
pub fn check_server_tree(tree: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    for (reached, path) in tree.reachable(0, |_, _| false) {
        let dep = &tree.packages[reached];
        if SERVER_FORBIDDEN.contains(&dep.name.as_str()) {
            errors.push(format!(
                "the server build reaches {}: {}",
                dep.name,
                tree.path_string(&path)
            ));
        }
    }
    errors
}

/// Crates the ECS-free core of `regia_net` never links (spec 120 § 3.2 and § 15.5, ADR-8032,
/// ADR-8501): the ECS, app, scene, asset and platform crates the `ecs` feature brings in, the audio
/// crate of `voice`, and the ECS library under `regia_ecs` (so a new path to it through another
/// crate is caught too).
pub const NET_CORE_FORBIDDEN: &[&str] = &[
    "regia_ecs",
    "regia_app",
    "regia_scene",
    "regia_asset",
    "regia_platform",
    "regia_audio",
    "bevy_ecs",
];

/// The package and `cargo tree` arguments of the net core's tree (what [`check_net_core_tree`] reads).
pub const NET_CORE_TREE: (&str, &[&str]) = ("regia_net", &["--no-default-features"]);

/// The no-ECS dependency assertion (spec 120 § 15.5): the resolved normal + build tree of
/// `regia_net --no-default-features` (root = package 0) reaches none of [`NET_CORE_FORBIDDEN`].
pub fn check_net_core_tree(tree: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    for (reached, path) in tree.reachable(0, |_, _| false) {
        let dep = &tree.packages[reached];
        if NET_CORE_FORBIDDEN.contains(&dep.name.as_str()) {
            errors.push(format!(
                "the regia_net core (--no-default-features) reaches {}: {} (spec 120 § 3.2: only the `ecs` \
                 feature may)",
                dep.name,
                tree.path_string(&path)
            ));
        }
    }
    errors
}

/// Crates `regia_asset_pipeline --no-default-features` never links (ADR-7972): the `https://`
/// cache client and its TLS stack (`http_cache`), the test-only certificate generator, and the
/// FBX, USD, SVG and Vorbis importers. A player build that cooks glTF at run time builds that way.
pub const PIPELINE_RUNTIME_FORBIDDEN: &[&str] = &[
    "ureq",
    "rustls",
    "rcgen",
    "ring",
    "ufbx",
    "openusd",
    "resvg",
    "usvg",
    "vorbis_rs",
];

/// Transitive rule on the resolved tree of `regia_asset_pipeline --no-default-features`.
pub fn check_pipeline_runtime_tree(tree: &Graph) -> Vec<String> {
    let mut errors = Vec::new();
    for (reached, path) in tree.reachable(0, |_, _| false) {
        let dep = &tree.packages[reached];
        if PIPELINE_RUNTIME_FORBIDDEN.contains(&dep.name.as_str()) {
            errors.push(format!(
                "regia_asset_pipeline --no-default-features reaches {} (ADR-7972: move it behind a default feature): {}",
                dep.name,
                tree.path_string(&path)
            ));
        }
    }
    errors
}

/// Adds the versions of coupled crates in one runtime crate's resolved tree to `into`.
pub fn collect_coupled(tree: &Graph, into: &mut BTreeMap<String, BTreeSet<String>>) {
    for reached in tree.reachable(0, skip_allowed(tree)).into_keys() {
        let dep = &tree.packages[reached];
        if COUPLED.contains(&dep.name.as_str()) {
            into.entry(dep.name.clone())
                .or_default()
                .insert(dep.version.clone());
        }
    }
}

/// Returns every coupled crate that resolves to more than one version across runtime crates.
pub fn check_versions(versions: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    versions
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(name, v)| {
            format!(
                "{name} resolves to {} versions in runtime crates: {}",
                v.len(),
                v.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> (Graph, usize, usize, usize) {
        let mut g = Graph::default();
        let core = g.add("regia_core", "0.1.0", true);
        let render = g.add("regia_render", "0.1.0", true);
        let editor = g.add("regia_editor", "0.1.0", true);
        g.link(render, core, false);
        g.link(editor, render, false);
        (g, core, render, editor)
    }

    /// A resolved tree rooted at a runtime crate (package 0).
    fn tree(root: &str) -> Graph {
        let mut g = Graph::default();
        g.add(root, "0.1.0", true);
        g
    }

    #[test]
    fn clean_graph_passes() {
        let (g, ..) = base();
        assert_eq!(check_direct(&g), Vec::<String>::new());
    }

    #[test]
    fn upward_edge_fails() {
        let (mut g, core, render, _) = base();
        g.link(core, render, false);
        let errors = check_direct(&g);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("regia_core (Foundation) must not depend on regia_render"));
    }

    #[test]
    fn same_layer_needs_allow_list() {
        let (mut g, _, render, _) = base();
        let physics = g.add("regia_physics", "0.1.0", true);
        let ui = g.add("regia_ui", "0.1.0", true);
        g.link(ui, render, false);
        assert!(check_direct(&g).is_empty());
        g.link(physics, render, false);
        assert_eq!(check_direct(&g).len(), 1);
    }

    #[test]
    fn editor_only_crates_used_directly_fail() {
        let (mut g, _, render, editor) = base();
        let libloading = g.add("libloading", "*", false);
        g.link(editor, libloading, false);
        assert!(check_direct(&g).is_empty());
        g.link(render, libloading, false);
        assert_eq!(
            check_direct(&g),
            vec!["regia_render uses libloading directly; only regia_editor may"]
        );
    }

    #[test]
    fn xess_loader_exception_does_not_allow_other_editor_dependencies() {
        let (mut g, ..) = base();
        let xess = g.add("regia_xess", "0.1.0", true);
        let loader = g.add("libloading", "0.9", false);
        g.link(xess, loader, false);
        assert!(check_direct(&g).is_empty());
        let dialog = g.add("rfd", "0.1", false);
        g.link(xess, dialog, false);
        assert_eq!(
            check_direct(&g),
            vec!["regia_xess uses rfd directly; only regia_editor may"]
        );
    }

    #[test]
    fn test_utils_only_as_dev_dependency() {
        let (mut g, core, ..) = base();
        let tu = g.add("regia_test_utils", "0.1.0", true);
        let image = g.add("image", "*", false);
        g.link(tu, image, false);
        g.link(core, tu, true);
        assert!(check_direct(&g).is_empty(), "{:?}", check_direct(&g));
        g.link(core, tu, false);
        assert!(!check_direct(&g).is_empty());
    }

    #[test]
    fn showcase_is_a_sample() {
        assert_eq!(layer_of("showcase"), Some(Layer::Sample));
    }

    #[test]
    fn samples_use_runtime_crates_and_each_other_only() {
        let (mut g, core, _, editor) = base();
        let sample = g.add("hello_window", "0.1.0", true);
        let android = g.add("hello_window_android", "0.1.0", true);
        g.link(sample, core, false);
        g.link(android, sample, false);
        assert!(check_direct(&g).is_empty(), "{:?}", check_direct(&g));
        g.link(core, sample, false);
        g.link(sample, editor, false);
        let errors = check_direct(&g);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("must not depend on the sample")));
    }

    #[test]
    fn hot_fixture_may_name_the_editor_as_the_feature_anchor() {
        let (mut g, _, _, editor) = base();
        let sample = g.add("hot_fixture", "0.1.0", true);
        g.link(sample, editor, false);
        assert!(check_direct(&g).is_empty(), "{:?}", check_direct(&g));
    }

    #[test]
    fn unknown_member_fails() {
        let (mut g, ..) = base();
        g.add("regia_mystery", "0.1.0", true);
        assert_eq!(check_direct(&g).len(), 1);
    }

    #[test]
    fn runtime_reaching_editor_dependency_fails_with_path() {
        let mut t = tree("regia_render");
        let middle = t.add("some_dep", "1.0.0", false);
        let egui = t.add("egui", "0.36.2", false);
        t.link(0, middle, false);
        t.link(middle, egui, false);
        let errors = check_runtime_tree(&t);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("regia_render -> some_dep -> egui"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn runtime_may_reach_libloading_indirectly() {
        let mut t = tree("regia_render");
        let wgpu = t.add("wgpu", "30.0.1", false);
        let libloading = t.add("libloading", "0.8.9", false);
        t.link(0, wgpu, false);
        t.link(wgpu, libloading, false);
        assert!(check_runtime_tree(&t).is_empty());
    }

    /// ADR-7972: the slim pipeline tree fails on any optional importer or TLS crate, with its path.
    #[test]
    fn pipeline_runtime_tree_reaching_an_optional_crate_fails_with_path() {
        let mut t = tree("regia_asset_pipeline");
        let gltf = t.add("gltf", "1.4.1", false);
        t.link(0, gltf, false);
        assert!(check_pipeline_runtime_tree(&t).is_empty());
        let ureq = t.add("ureq", "2.12.1", false);
        let rustls = t.add("rustls", "0.23.46", false);
        t.link(0, ureq, false);
        t.link(ureq, rustls, false);
        let errors = check_pipeline_runtime_tree(&t);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("reaches rustls") && e.contains("ureq")),
            "{errors:?}"
        );
    }

    #[test]
    fn server_tree_reaching_a_client_crate_fails_with_path() {
        let mut t = tree("regia");
        let framework = t.add("regia_framework", "0.1.0", true);
        let wgpu = t.add("wgpu", "30.0.0", false);
        t.link(0, framework, false);
        assert!(check_server_tree(&t).is_empty());
        t.link(framework, wgpu, false);
        let errors = check_server_tree(&t);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("regia_framework"), "{errors:?}");
    }

    #[test]
    fn the_net_core_tree_may_not_reach_an_ecs_crate() {
        // The core as built (net-core-no-ecs-a): regia_core, regia_detsim and regia_macros only.
        let core_tree = || {
            let mut t = tree("regia_net");
            for (name, workspace) in [("regia_core", true), ("regia_detsim", true), ("snow", false)] {
                let dep = t.add(name, "0.5.0", workspace);
                t.link(0, dep, false);
            }
            t
        };
        let mut t = core_tree();
        assert!(
            check_net_core_tree(&t).is_empty(),
            "{:?}",
            check_net_core_tree(&t)
        );
        // The manifest before ADR-8032 linked regia_ecs, regia_app, regia_scene and regia_asset
        // directly: each is a finding with its path.
        let mut direct = core_tree();
        for name in ["regia_ecs", "regia_app", "regia_scene", "regia_asset"] {
            let dep = direct.add(name, "0.5.0", true);
            direct.link(0, dep, false);
        }
        let errors = check_net_core_tree(&direct);
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("regia_net -> regia_asset")),
            "{errors:?}"
        );
        // An indirect path to the ECS library through a core dependency is caught too.
        let core = t.add("regia_core", "0.5.0", true);
        let bevy = t.add("bevy_ecs", "0.20.0", false);
        t.link(core, bevy, false);
        let errors = check_net_core_tree(&t);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("regia_net -> regia_core -> bevy_ecs"),
            "{errors:?}"
        );
    }

    #[test]
    fn the_net_core_tree_is_read_without_default_features() {
        assert_eq!(NET_CORE_TREE, ("regia_net", &["--no-default-features"][..]));
        for name in [
            "regia_ecs",
            "regia_app",
            "regia_scene",
            "regia_asset",
            "regia_platform",
        ] {
            assert!(NET_CORE_FORBIDDEN.contains(&name), "{name}");
        }
    }

    #[test]
    fn runtime_reaching_desktop_member_fails() {
        let mut t = tree("regia");
        let pipeline = t.add("regia_asset_pipeline", "0.1.0", true);
        t.link(0, pipeline, false);
        assert_eq!(check_runtime_tree(&t).len(), 1);
    }

    #[test]
    fn runtime_reaching_the_reference_path_tracer_fails_with_path() {
        // specs/27 § 1.1 / § 6: the tracer never ships in a game binary.
        let mut t = tree("regia");
        let agent = t.add("regia_agent", "0.1.0", true);
        let pt = t.add("regia_pt", "0.1.0", true);
        t.link(0, agent, false);
        assert!(check_runtime_tree(&t).is_empty());
        t.link(agent, pt, false);
        let errors = check_runtime_tree(&t);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("reaches regia_pt: regia -> regia_agent -> regia_pt"),
            "{errors:?}"
        );
        // A sample is a game too.
        let mut game = tree("showcase");
        let pt = game.add("regia_pt", "0.1.0", true);
        game.link(0, pt, false);
        assert_eq!(check_runtime_tree(&game).len(), 1);
    }

    #[test]
    fn only_the_listed_offline_tool_sample_may_link_the_tracer() {
        assert_eq!(OFFLINE_ONLY_ROOTS, [("lookdev", "regia_pt")]);
        let mut t = tree("lookdev");
        let pt = t.add("regia_pt", "0.1.0", true);
        let render = t.add("regia_render", "0.1.0", true);
        t.link(0, pt, false);
        t.link(pt, render, false);
        assert!(check_runtime_tree(&t).is_empty());
        // The tracer's own tree is checked as a runtime member and passes.
        let mut own = tree("regia_pt");
        let render = own.add("regia_render", "0.1.0", true);
        own.link(0, render, false);
        assert!(check_runtime_tree(&own).is_empty());
    }

    /// A declared graph with the agent server's tool-only feature table (as in its Cargo.toml).
    fn declared_with_agent() -> (Graph, usize, usize) {
        let mut g = Graph::default();
        let agent = g.add("regia_agent", "0.1.0", true);
        let pt = g.add("regia_pt", "0.1.0", true);
        g.link(agent, pt, false);
        g.optional.insert((agent, pt));
        let table = [
            ("reference-trace", vec!["dep:regia_pt"]),
            ("denoise", vec!["reference-trace", "regia_pt/denoise"]),
            ("dcc-feed", vec![]),
            ("default", vec![]),
        ];
        g.features.insert(
            agent,
            table
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.into_iter().map(str::to_string).collect()))
                .collect(),
        );
        (g, agent, pt)
    }

    #[test]
    fn spec27_1_1_a_runtime_crate_declaring_the_tracer_fails() {
        // specs/27 § 1.1: regia_pt is never a dependency of a runtime crate, optional or not.
        let (mut g, _, pt) = declared_with_agent();
        assert_eq!(check_offline_only(&g), Vec::<String>::new());
        let framework = g.add("regia_framework", "0.1.0", true);
        g.link(framework, pt, false);
        g.optional.insert((framework, pt));
        let errors = check_offline_only(&g);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0]
                .contains("runtime crate regia_framework declares a dependency on the offline-only regia_pt"),
            "{errors:?}"
        );
        // The agent's edge must stay optional.
        let (mut g, agent2, pt2) = declared_with_agent();
        g.optional.remove(&(agent2, pt2));
        let errors = check_offline_only(&g);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("it must stay optional"), "{errors:?}");
        // A game sample is a runtime crate too; dev-only edges (tests) are not shipped.
        let (mut g, ..) = declared_with_agent();
        let game = g.add("showcase", "0.1.0", true);
        let tests_only = g.add("regia_render", "0.1.0", true);
        g.link(game, pt, false);
        g.link(tests_only, pt, true);
        let errors = check_offline_only(&g);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("runtime crate showcase"), "{errors:?}");
    }

    #[test]
    fn spec27_1_1_lookdev_and_the_tools_may_link_the_tracer() {
        let (mut g, _, pt) = declared_with_agent();
        for tool in ["lookdev", "regia_editor", "regia_cli", "render_lab", "xtask"] {
            let at = g.add(tool, "0.1.0", true);
            g.link(at, pt, false);
        }
        assert_eq!(check_offline_only(&g), Vec::<String>::new());
    }

    #[test]
    fn spec27_1_1_a_runtime_crate_turning_on_the_tracer_feature_fails() {
        let (mut g, ..) = declared_with_agent();
        let game = g.add("showcase", "0.1.0", true);
        let framework = g.add("regia_framework", "0.1.0", true);
        let editor = g.add("regia_editor", "0.1.0", true);
        let lookdev = g.add("lookdev", "0.1.0", true);
        g.feature_refs.insert(game, vec!["regia_agent/dcc-feed".into()]);
        g.feature_refs
            .insert(editor, vec!["regia_agent/reference-trace".into()]);
        g.feature_refs.insert(lookdev, vec!["regia_agent/denoise".into()]);
        assert_eq!(check_offline_only(&g), Vec::<String>::new());
        // `denoise` turns `reference-trace` on, so it links the tracer too; `?/` is the same.
        g.feature_refs.insert(game, vec!["regia_agent/denoise".into()]);
        g.feature_refs
            .insert(framework, vec!["regia_agent?/reference-trace".into()]);
        let errors = check_offline_only(&g);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("runtime crate showcase turns on regia_agent/denoise")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("runtime crate regia_framework turns on regia_agent/reference-trace")),
            "{errors:?}"
        );
    }

    #[test]
    fn spec27_1_1_the_agent_default_features_must_not_link_the_tracer() {
        let (mut g, agent, _) = declared_with_agent();
        g.features
            .get_mut(&agent)
            .expect("table")
            .insert("default".into(), vec!["denoise".into()]);
        let errors = check_offline_only(&g);
        assert_eq!(
            errors,
            vec!["regia_agent's default features turn on the offline-only regia_pt (specs/27 § 1.1)"]
        );
    }

    #[test]
    fn runtime_reaching_ufbx_fails() {
        // The FBX importer's C parser is pipeline-only (ADR-1602).
        let mut t = tree("regia_scene");
        let ufbx = t.add("ufbx", "0.11.5", false);
        t.link(0, ufbx, false);
        assert_eq!(check_runtime_tree(&t).len(), 1);
    }

    #[test]
    fn runtime_reaching_openusd_fails() {
        // The USD importer's reader is pipeline-only (ADR-3235).
        let mut t = tree("regia_scene");
        let usd = t.add("openusd", "0.8.0", false);
        t.link(0, usd, false);
        assert_eq!(check_runtime_tree(&t).len(), 1);
    }

    #[test]
    fn asset_dev_feature_may_use_notify() {
        let mut t = tree("regia_scene");
        let asset = t.add("regia_asset", "0.1.0", true);
        let notify = t.add("notify", "8.2.0", false);
        t.link(0, asset, false);
        t.link(asset, notify, false);
        assert!(check_runtime_tree(&t).is_empty());
        t.link(0, notify, false);
        assert_eq!(check_runtime_tree(&t).len(), 1);
    }

    #[test]
    fn image_is_allowed_for_showcase_only() {
        let users: Vec<&str> = ALLOWED_RUNTIME_EDGES
            .iter()
            .filter(|(_, dep)| *dep == "image")
            .map(|(user, _)| *user)
            .collect();
        assert_eq!(users, ["showcase"]);

        let mut t = tree("showcase");
        let image = t.add("image", "0.25.0", false);
        t.link(0, image, false);
        assert!(check_runtime_tree(&t).is_empty());

        let mut other = tree("lookdev");
        let image = other.add("image", "0.25.0", false);
        other.link(0, image, false);
        assert_eq!(check_runtime_tree(&other).len(), 1);
    }

    #[test]
    fn duplicate_coupled_version_across_runtime_crates_fails() {
        let mut a = tree("regia_core");
        let g33 = a.add("glam", "0.33.12", false);
        a.link(0, g33, false);
        let mut b = tree("regia_render");
        let g32 = b.add("glam", "0.32.1", false);
        b.link(0, g32, false);
        let mut versions = BTreeMap::new();
        collect_coupled(&a, &mut versions);
        assert!(check_versions(&versions).is_empty());
        collect_coupled(&b, &mut versions);
        assert_eq!(
            check_versions(&versions),
            vec!["glam resolves to 2 versions in runtime crates: 0.32.1, 0.33.12"]
        );
    }
}
