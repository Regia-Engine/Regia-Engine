// SPDX-License-Identifier: MPL-2.0
//! One asset-open dispatcher (`docs/specs/19-editor.md` § 2, ADR-7725): what a double click in the
//! Content Browser, a click on an asset link in the Output Log and `editor.open_asset` do with an
//! asset path.
//!
//! [`OPEN_KINDS`] is the table: each authoring kind's suffixes, the panel it opens in and the route.
//! Most kinds open from the world alone, through the same agent method an agent would call for that
//! panel (`editor.bt_open`, `editor.vfx_open`, …), so the three entry points open a file exactly the
//! way the panel's own method does and answer the same codes. Three kinds live on the editor shell
//! rather than in the world: a `.material.ron` (the material editor), the Asset Viewer's kinds and a
//! level (the open level is swapped by the shell). Those queue a [`crate::log_links::AssetOpenRequests`]
//! entry that the shell finishes before the panels draw on its next frame.
//!
//! A path no row names answers `asset_kind_unknown` (the hint lists the kinds); an audio patch
//! (`.audiopatch.ron`) has no editor panel, so it is one of those.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::shell::Panel;
use crate::viewport_agent::Handler;

/// How a kind's path reaches its panel's open method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRoute {
    /// The agent method `method` with `{ <key>: path }`; the path is project-relative (`assets/…`),
    /// or relative to `assets/` when `under_assets` is set.
    Method {
        /// The method's name (`editor.bt_open`).
        method: &'static str,
        /// The parameter that carries the path (`path`, or `asset` for a montage).
        key: &'static str,
        /// The method reads the path relative to `assets/` (the Tables and Abilities panels).
        under_assets: bool,
    },
    /// Prefab edit mode (`crate::prefab_edit::enter_prefab_edit`).
    Prefab,
    /// The shell's material editor.
    Material,
    /// The shell's Asset Viewer.
    Viewer,
    /// The shell swaps the open level (refused while it has unsaved edits).
    Level,
}

/// One openable kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenKind {
    /// Stable wire name (`behavior_tree`).
    pub name: &'static str,
    /// File suffixes, lowercase, with the leading dot (`.bt.ron`). Empty for the viewer kinds,
    /// which [`crate::asset_viewer::kind_of`] decides.
    pub suffixes: &'static [&'static str],
    /// The panel the file opens in (`None`: prefab edit mode and levels show in the viewport, a data
    /// asset in its own window).
    pub panel: Option<Panel>,
    /// How it opens.
    pub route: OpenRoute,
    /// The agent method that reports the open path, and the field of its answer that holds it
    /// (`None`: the material editor, the Asset Viewer and the Data Asset window have no state
    /// method; their open path shows in the shell).
    pub state: Option<(&'static str, &'static str)>,
}

const fn method(name: &'static str) -> OpenRoute {
    OpenRoute::Method {
        method: name,
        key: "path",
        under_assets: false,
    }
}

const fn method_under_assets(name: &'static str) -> OpenRoute {
    OpenRoute::Method {
        method: name,
        key: "path",
        under_assets: true,
    }
}

/// Every kind the dispatcher opens, matched in this order (the first row whose suffix ends the path
/// wins; the viewer row last).
pub const OPEN_KINDS: &[OpenKind] = &[
    OpenKind {
        name: "level",
        suffixes: &[".level.ron"],
        panel: Some(Panel::Viewport),
        route: OpenRoute::Level,
        state: Some(("editor.headless_status", "level")),
    },
    OpenKind {
        name: "prefab",
        suffixes: &[".prefab.ron", ".variant.ron"],
        panel: Some(Panel::Viewport),
        route: OpenRoute::Prefab,
        state: Some(("editor.prefab_mode", "editing")),
    },
    OpenKind {
        name: "material",
        suffixes: &[".material.ron"],
        panel: Some(Panel::Material),
        route: OpenRoute::Material,
        state: None,
    },
    OpenKind {
        name: "material_graph",
        suffixes: &[".matgraph.ron"],
        panel: Some(Panel::MaterialGraph),
        route: method("editor.matgraph_open"),
        state: Some(("editor.matgraph_state", "path")),
    },
    OpenKind {
        name: "material_function",
        suffixes: &[".matfunc.ron"],
        panel: Some(Panel::MaterialGraph),
        route: method("editor.matgraph_function_open"),
        state: Some(("editor.matgraph_state", "path")),
    },
    OpenKind {
        name: "material_instance",
        suffixes: &[".matinst.ron"],
        panel: Some(Panel::MaterialGraph),
        route: method("editor.matgraph_instance_open"),
        state: Some(("editor.matgraph_state", "path")),
    },
    OpenKind {
        name: "anim_graph",
        suffixes: &[".animgraph.ron"],
        panel: Some(Panel::AnimGraph),
        route: method("editor.anim_open"),
        state: Some(("editor.anim_graph", "path")),
    },
    OpenKind {
        name: "procgen_graph",
        suffixes: &[".pgraph.ron", ".pgvar.ron"],
        panel: Some(Panel::ProcGraph),
        route: method("editor.procgen_open"),
        state: Some(("editor.procgen_state", "path")),
    },
    OpenKind {
        name: "ability",
        suffixes: &[".ability.ron", ".effect.ron", ".attrs.ron"],
        panel: Some(Panel::Abilities),
        route: method_under_assets("editor.abilities_panel_asset"),
        state: Some(("editor.abilities_panel_get", "asset.path")),
    },
    OpenKind {
        name: "behavior_tree",
        suffixes: &[".bt.ron"],
        panel: Some(Panel::BehaviorTree),
        route: method("editor.bt_open"),
        state: Some(("editor.bt_state", "path")),
    },
    OpenKind {
        name: "state_tree",
        suffixes: &[".statetree.ron"],
        panel: Some(Panel::StateTreeEditor),
        route: method("editor.statetree_open"),
        state: Some(("editor.statetree_state", "path")),
    },
    OpenKind {
        name: "montage",
        suffixes: &[".montage.ron", ".montagevar.ron"],
        panel: Some(Panel::Montage),
        route: OpenRoute::Method {
            method: "editor.montage_panel_open",
            key: "asset",
            under_assets: false,
        },
        state: Some(("editor.montage_panel_get", "asset")),
    },
    OpenKind {
        name: "sequence",
        suffixes: &[".seq.ron"],
        panel: Some(Panel::SeqCurves),
        route: method("editor.seq_curve_open"),
        state: Some(("editor.seq_curve_state", "path")),
    },
    OpenKind {
        name: "shot_list",
        suffixes: &[".shots.ron"],
        panel: Some(Panel::Shots),
        route: method("editor.shot_panel_open"),
        state: Some(("editor.shot_panel_state", "path")),
    },
    OpenKind {
        name: "ui_layout",
        suffixes: &[".ui.ron"],
        panel: Some(Panel::UiDesigner),
        route: method("editor.ui_designer_open"),
        state: Some(("editor.ui_designer_state", "path")),
    },
    OpenKind {
        name: "ui_animation",
        suffixes: &[".uianim.ron"],
        panel: Some(Panel::UiAnim),
        route: method("editor.ui_anim_open"),
        state: Some(("editor.ui_anim_list", "path")),
    },
    OpenKind {
        name: "sound_cue",
        suffixes: &[".cue.ron"],
        panel: Some(Panel::CueEditor),
        route: method("editor.cue_edit"),
        state: Some(("editor.cue_edit", "path")),
    },
    OpenKind {
        name: "data_asset",
        suffixes: &[".data.ron"],
        panel: None,
        route: method("editor.data_asset_open"),
        state: None,
    },
    OpenKind {
        name: "data_table",
        suffixes: &[".table.ron", ".table.csv"],
        panel: Some(Panel::Tables),
        route: method_under_assets("editor.tables_panel_open"),
        state: Some(("editor.tables_panel_get", "data.path")),
    },
    OpenKind {
        name: "curve_table",
        suffixes: &[".curves.ron", ".curves.csv"],
        panel: Some(Panel::Tables),
        route: method_under_assets("editor.tables_panel_open"),
        state: Some(("editor.tables_panel_get", "curves.path")),
    },
    OpenKind {
        name: "chooser",
        suffixes: &[".chooser.ron"],
        panel: Some(Panel::Tables),
        route: method_under_assets("editor.tables_panel_open"),
        state: Some(("editor.tables_panel_get", "chooser.path")),
    },
    OpenKind {
        name: "conversation",
        suffixes: &[".conv.ron"],
        panel: Some(Panel::Conversation),
        route: method("editor.conversation_open"),
        state: Some(("editor.conversation_graph", "path")),
    },
    OpenKind {
        name: "vfx_system",
        suffixes: &[".vfx.ron"],
        panel: Some(Panel::VfxEditor),
        route: method("editor.vfx_open"),
        state: Some(("editor.vfx_state", "path")),
    },
    OpenKind {
        name: "video",
        suffixes: crate::video_panel::VIDEO_SUFFIXES,
        panel: Some(Panel::Video),
        route: method("editor.video_open"),
        state: Some(("editor.video_state", "source")),
    },
    // w11-forms-undo-autosave's New kinds (ADR-7731; added at integ-v06i, ADR-7774): a cloth and a soft
    // body open in the Cloth panel through `editor.cloth_panel_edit { path }`.
    OpenKind {
        name: "cloth",
        suffixes: &[".cloth.ron"],
        panel: Some(Panel::Cloth),
        route: method("editor.cloth_panel_edit"),
        state: Some(("editor.cloth_panel_state", "path")),
    },
    OpenKind {
        name: "soft_body",
        suffixes: &[".soft.ron"],
        panel: Some(Panel::Cloth),
        route: method("editor.cloth_panel_edit"),
        state: Some(("editor.cloth_panel_state", "path")),
    },
    // w11-motiondb-editor-a's New kind (ADR-7740; added at integ-v06j, review sprint-w11-rev-a-k #10):
    // a motion database opens in its own editor window through `editor.mm_db_open { path }`.
    OpenKind {
        name: "motion_database",
        suffixes: &[".motiondb.ron"],
        panel: None,
        route: method("editor.mm_db_open"),
        state: Some(("editor.mm_db_state", "path")),
    },
    // w11-biome-procgen-tooling's New kind (added at integ-v06j): a biome opens in the Biome editor
    // through `editor.biome_panel_open { path }`.
    OpenKind {
        name: "biome",
        suffixes: &[".biome.ron"],
        panel: Some(Panel::Biome),
        route: method("editor.biome_panel_open"),
        state: Some(("editor.biome_panel_get", "path")),
    },
    // Kinds added after ADR-7725 (w11 editor lanes): append ONE row each above this comment's
    // viewer row, keeping the viewer row last.
    // w11-logic-graph-finish: a logic graph opens in the Logic Graph panel through
    // `editor.logic_open { path }` (its `.logic.layout.ron` sidecar matches no row).
    OpenKind {
        name: "logic_graph",
        suffixes: &[".logic.ron"],
        panel: Some(Panel::LogicGraph),
        route: method("editor.logic_open"),
        state: Some(("editor.logic_state", "path")),
    },
    OpenKind {
        name: "viewer",
        suffixes: &[],
        panel: Some(Panel::AssetViewer),
        route: OpenRoute::Viewer,
        state: None,
    },
];

/// The kind `path` opens as, by suffix (case-insensitive).
#[must_use]
pub fn kind_of(path: &str) -> Option<&'static OpenKind> {
    let lower = path.to_ascii_lowercase();
    OPEN_KINDS.iter().find(|kind| match kind.route {
        OpenRoute::Viewer => crate::asset_viewer::kind_of(&lower).is_some(),
        _ => kind.suffixes.iter().any(|suffix| lower.ends_with(suffix)),
    })
}

/// The wire names of every openable kind, in table order.
#[must_use]
pub fn kind_names() -> Vec<&'static str> {
    OPEN_KINDS.iter().map(|kind| kind.name).collect()
}

/// The `asset_kind_unknown` refusal for `path`.
#[must_use]
pub fn unknown_kind(path: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "asset_kind_unknown",
        format!("no editor panel opens {path}"),
        format!(
            "openable kinds: {} (see editor.asset_kinds)",
            kind_names().join(", ")
        ),
    )
}

/// What [`open`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    /// The kind.
    pub kind: &'static OpenKind,
    /// The project-relative path (`assets/…`).
    pub path: String,
    /// The panel method's answer (`None` when the shell finishes the open next frame).
    pub reply: Option<Value>,
}

impl Opened {
    /// `true` when the shell finishes the open on its next frame.
    #[must_use]
    pub fn queued(&self) -> bool {
        self.reply.is_none()
    }

    /// The `editor.open_asset` answer.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "kind": self.kind.name,
            "panel": self.kind.panel.map(Panel::title),
            "queued": self.queued(),
            "reply": self.reply,
            "state": self.kind.state.map(|(method, field)| json!({ "method": method, "field": field })),
        })
    }
}

/// `path` as the project-relative `assets/…` form: a leading `./` and backslashes are normalized,
/// and a path without the `assets/` prefix gets it.
#[must_use]
pub fn project_relative(path: &str) -> String {
    let rel = path.replace('\\', "/");
    let rel = rel.trim_start_matches("./");
    if rel.starts_with("assets/") || rel.starts_with("builtin:") {
        rel.to_owned()
    } else {
        format!("assets/{rel}")
    }
}

/// The agent handler named `name`: the world's registered extension first, else the editor's list.
fn handler_of(world: &World, name: &str) -> Option<Handler> {
    if let Some(extension) = world.get_resource::<regia_agent::extension::AgentExtension>()
        && let Some((_, handler)) = extension.method(name)
    {
        return Some(handler);
    }
    crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == name)
        .map(|(_, handler)| handler)
}

/// Opens `path` (project-relative, `assets/…` or under `assets/`) in its panel. World kinds open now
/// and the panel is brought forward; the shell kinds (material, viewer, level) are queued for the
/// shell's next frame ([`crate::log_links::route_asset_requests`]).
///
/// # Errors
///
/// `asset_kind_unknown`; `F0003` when the file does not exist; the panel method's own codes.
pub fn open(world: &mut World, path: &str) -> Result<Opened, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "asset_open", path).entered();
    let rel = project_relative(path);
    let kind = kind_of(&rel).ok_or_else(|| unknown_kind(&rel))?;
    let root = crate::agent_ext::agent_root(world);
    if !rel.starts_with("builtin:") {
        let full = regia_agent::req_path::project_path_in(&root, "path", &rel)?;
        if !full.is_file() {
            return Err(RpcError::not_found(
                format!("no file {rel}"),
                "the path is project-relative (assets/…); see editor.list_assets",
            ));
        }
    }
    let reply = match kind.route {
        OpenRoute::Material | OpenRoute::Viewer | OpenRoute::Level => {
            world
                .get_resource_or_insert_with(crate::log_links::AssetOpenRequests::default)
                .paths
                .push(rel.clone());
            None
        }
        OpenRoute::Prefab => {
            crate::prefab_edit::enter_prefab_edit(world, &root.join(&rel)).map_err(|message| {
                RpcError::new(
                    INVALID_PARAMS,
                    "asset_open_failed",
                    message,
                    "see editor.open_prefab",
                )
            })?;
            Some(json!({ "editing": rel }))
        }
        OpenRoute::Method {
            method,
            key,
            under_assets,
        } => {
            let handler = handler_of(world, method)
                .ok_or_else(|| RpcError::internal(format!("{method} is not registered")))?;
            let arg = if under_assets {
                rel.strip_prefix("assets/").unwrap_or(&rel).to_owned()
            } else {
                rel.clone()
            };
            let mut params = serde_json::Map::new();
            params.insert(key.to_owned(), Value::String(arg));
            let reply = handler(world, &Value::Object(params))?;
            if let Some(panel) = kind.panel {
                crate::editor_layout::LayoutRequests::push(
                    world,
                    crate::editor_layout::LayoutRequest::Open(panel),
                );
            }
            Some(reply)
        }
    };
    Ok(Opened {
        kind,
        path: rel,
        reply,
    })
}

/// The refusals that hold whatever the level's edits: a prefab being edited
/// (`asset_open_prefab_editing`) and a running game (`asset_open_play_running`).
fn level_switch_blocker(world: &World, rel: &str) -> Result<(), RpcError> {
    let refuse = |code: &str, message: String| RpcError::new(INVALID_PARAMS, code, message, "");
    if world
        .get_resource::<crate::prefab_edit::PrefabEditMode>()
        .is_some_and(|mode| mode.editing().is_some())
    {
        return Err(refuse(
            "asset_open_prefab_editing",
            format!("Close the prefab before opening {rel}"),
        ));
    }
    if world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|pie| pie.mode.is_some())
    {
        return Err(refuse(
            "asset_open_play_running",
            format!("Stop play-in-editor before opening {rel}"),
        ));
    }
    Ok(())
}

/// Whether the open level has edits that are not saved.
fn level_has_unsaved_edits(world: &World) -> bool {
    world
        .get_resource::<crate::command::UndoStack>()
        .is_some_and(crate::command::UndoStack::is_dirty)
}

/// Opens a level the shell queued: refused over a prefab being edited, a running game or unsaved
/// level edits (the click must not throw them away).
///
/// # Errors
///
/// `asset_open_prefab_editing`, `asset_open_play_running`, `asset_open_level_unsaved`,
/// `asset_open_failed` (the file does not load).
pub fn open_level_guarded(world: &mut World, rel: &str) -> Result<(), RpcError> {
    let refuse = |code: &str, message: String| RpcError::new(INVALID_PARAMS, code, message, "");
    level_switch_blocker(world, rel)?;
    if level_has_unsaved_edits(world) {
        return Err(refuse(
            "asset_open_level_unsaved",
            format!("Save the level before opening {rel}"),
        ));
    }
    let full = crate::agent_ext::agent_root(world).join(rel);
    let level = crate::level_io::open_level_file(world, &full)
        .map_err(|message| refuse("asset_open_failed", message))?;
    world.insert_resource(level);
    crate::level_io::reset_edit_state(world);
    Ok(())
}

/// Finishes a queued open on the shell's side: a material and a viewer kind become the Content
/// Browser's requests, a level opens behind [`open_level_guarded`], and any other kind opens through
/// [`open`]. Answers the status line of a refusal.
pub fn open_in_shell(
    world: &mut World,
    content: &mut crate::content::ContentBrowserState,
    rel: &str,
) -> Option<String> {
    let kind = kind_of(rel)?;
    match kind.route {
        OpenRoute::Material => {
            content.open_request = Some(crate::agent_ext::agent_root(world).join(rel));
            None
        }
        OpenRoute::Viewer => {
            content.view_request = Some(rel.to_owned());
            None
        }
        OpenRoute::Level => open_level_guarded(world, rel).err().map(|e| e.message),
        _ => open(world, rel).err().map(|e| e.message),
    }
}

/// What a double click on `rel` in the Content Browser does: the same dispatcher, with the level
/// swapped into `open_level` (the panel holds it while it draws) and the shell kinds handed to
/// `state`'s requests. Answers a toast line when the file could not open.
pub fn open_from_browser(
    world: &mut World,
    state: &mut crate::content::ContentBrowserState,
    open_level: &mut crate::level_io::OpenLevel,
    rel: &str,
) -> Option<String> {
    let kind = kind_of(rel)?;
    let root = state.project_root.clone()?;
    match kind.route {
        OpenRoute::Level => {
            // The agent route's guards (prefab edit, running game), then the shell's rule for a
            // level switch: unsaved edits ask first through the Save all dialog, which opens the
            // level after **Save selected** or **Don't save** (ADR-7822).
            if let Err(err) = level_switch_blocker(world, rel) {
                let code = err
                    .data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Some(format!("{} ({code})", err.message));
            }
            let full = root.join(rel);
            if level_has_unsaved_edits(world) {
                crate::save_all_dialog::open_dialog(
                    world,
                    crate::save_all_dialog::SavePurpose::LevelSwitch(
                        crate::save_all_dialog::LevelSwitch::Open(full),
                    ),
                );
                return Some("Unsaved changes: choose what to save".into());
            }
            match crate::level_io::open_level_file(world, &full) {
                Ok(level) => {
                    *open_level = level;
                    crate::level_io::reset_edit_state(world);
                    None
                }
                Err(err) => Some(err),
            }
        }
        OpenRoute::Material => {
            state.open_request = Some(root.join(rel));
            None
        }
        OpenRoute::Viewer => {
            state.view_request = Some(rel.to_owned());
            None
        }
        _ => match open(world, rel) {
            Ok(_) => None,
            Err(err) => Some(
                match err
                    .data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str)
                {
                    Some(code) => format!("{} ({code})", err.message),
                    None => err.message,
                },
            ),
        },
    }
}

/// `editor.open_asset { path }`.
fn open_asset_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, assets/…) is required"))?;
    open(world, path).map(|opened| opened.to_json())
}

/// `editor.asset_kinds`: what `editor.open_asset` opens and what `editor.create_asset` creates.
fn asset_kinds_method(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let open: Vec<Value> = OPEN_KINDS
        .iter()
        .map(|kind| {
            json!({
                "kind": kind.name,
                "suffixes": kind.suffixes,
                "panel": kind.panel.map(Panel::title),
                "state": kind.state.map(|(method, field)| json!({ "method": method, "field": field })),
            })
        })
        .collect();
    let create: Vec<Value> = crate::content::NewAssetKind::ALL
        .iter()
        .map(|kind| {
            json!({
                "kind": kind.as_str(),
                "label": kind.label(),
                "extension": kind.extension(),
            })
        })
        .collect();
    Ok(json!({ "open": open, "create": create }))
}

/// The asset-open agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.open_asset",
                tool: "open_asset",
                description: "Open an asset in its editor panel, as a double click in the Content Browser does: levels, prefabs, materials, material graphs, functions and instances, anim graphs, procgen graphs and variants, abilities, effects and attribute sets, behavior and state trees, montages, sequences, shot lists, UI layouts and animations, sound cues, data assets, data tables, curve tables, choosers, conversations, VFX systems, video, cloths and soft bodies, motion databases, biomes, logic graphs and the Asset Viewer's meshes, textures and sounds. Materials, viewer kinds and levels open on the shell's next frame (queued: true).",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative asset path (assets/…).",
                }],
                result: "{ path, kind, panel, queued, reply, state: { method, field }? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            open_asset_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_kinds",
                tool: "asset_kinds",
                description: "The kinds editor.open_asset opens (suffixes, panel, the state method and field that report the open path) and the kinds editor.create_asset creates (the Content Browser's New menu, in its order).",
                params: &[],
                result: "{ open: [{ kind, suffixes, panel, state? }], create: [{ kind, label, extension }] }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_kinds_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
