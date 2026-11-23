// SPDX-License-Identifier: MPL-2.0
//! The procedural graph editor's agent surface (`editor.procgen_*`, spec 67 § 6, page
//! `book/agents/procgen-editor.md`).
//!
//! Errors answer `F0002` (invalid params) or `F0003` (missing document, node or target) with
//! `data.reason` set to a lowercase code listed on the book page. Graph content edits use
//! `procgen.graph_edit`, which lands in the open document (and its undo stack) when the graph is
//! open here.

use std::collections::BTreeSet;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::debug_view::{self, ColorBy, ProcPreview, VisForm, VisualizeStyle};
use super::doc::{self, DocError, ProcGraphDoc};
use super::inspect::{self, ProcInspect};
use super::tools::{self, Brush, ToolGesture, ToolKind};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Codes that mean "not found" (`F0003`); the rest are `F0002`.
const NOT_FOUND_CODES: &[&str] = &[
    "procgen_not_open",
    "procgen_no_preview",
    "procgen_unknown_node",
    "procgen_unknown_graph",
    "procgen_not_generator",
    "procgen_no_debug_data",
    "procgen_debug_dropped",
    "procgen_no_inspect",
    "procgen_no_conflict",
    "procgen_no_point",
];

fn rpc(e: DocError) -> RpcError {
    let (code, regia) = if NOT_FOUND_CODES.contains(&e.code.as_str()) {
        (NOT_FOUND, "F0003")
    } else {
        (INVALID_PARAMS, "F0002")
    };
    RpcError {
        code,
        message: e.message,
        data: Some(json!({ "code": regia, "reason": e.code, "hint": "see book/agents/procgen-editor.md" })),
    }
}

fn bad(why: &str, message: impl Into<String>) -> RpcError {
    rpc(DocError::new(why, message))
}

const fn p(name: &'static str, kind: &'static str, required: bool, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const fn m(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
    permission: Permission,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating: !matches!(permission, Permission::Read),
        permission,
    }
}

/// Every `editor.procgen_*` method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.procgen_open",
                "procgen_open",
                "Open a .pgraph.ron (or a .pgvar.ron, parent read-only) in the procedural graph panel; enter opens the graph a graph.call/graph.loop node calls (breadcrumb), back returns.",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            false,
                            "Project-relative graph or variant path (clears the breadcrumb).",
                        ),
                        p(
                            "enter",
                            "integer",
                            false,
                            "A graph.call / graph.loop node of the open graph to open.",
                        ),
                        p(
                            "back",
                            "boolean",
                            false,
                            "Return to the graph above in the breadcrumb.",
                        ),
                        p(
                            "generator",
                            "integer",
                            false,
                            "Open this generator's graph and preview on it (the inspector's Open graph).",
                        ),
                    ]
                },
                "{ path, graph_hash, errors: [{node, reason, message}], variant, breadcrumb }",
                Permission::Read,
            ),
            open as Handler,
        ),
        (
            m(
                "editor.procgen_new",
                "procgen_new",
                "Create a .pgraph.ron from a starter graph or blank (in.self → sample.region → out.points) and open it.",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            true,
                            "New project-relative .pgraph.ron path (must not exist).",
                        ),
                        p(
                            "starter",
                            "string",
                            false,
                            "A graph whose flags.starter is true to copy.",
                        ),
                    ]
                },
                "{ path, graph_hash, errors }",
                Permission::Assets,
            ),
            new as Handler,
        ),
        (
            m(
                "editor.procgen_starters",
                "procgen_starters",
                "The New procedural graph picker: the project's starter graphs (flags.starter); empty with enabled: false when the picker is off in editor preferences. With starter set, marks the open graph as a starter or not (Make starter, one undo entry; that form needs the edit_scene permission, F0012 otherwise).",
                &const {
                    [p(
                        "starter",
                        "boolean",
                        false,
                        "Set the open graph's flags.starter (Make starter).",
                    )]
                },
                "{ enabled, starters: [{path, tool, domain, description}] }",
                Permission::Read,
            ),
            starters as Handler,
        ),
        (
            m(
                "editor.procgen_state",
                "procgen_state",
                "The panel's state: open document, dirty flag, pending reload choice, selection, preview target and its run, visualized nodes, undo depth.",
                &[],
                "{ path, dirty, read_only, conflict, selection, errors, preview: {generator, state, runs, cancelled, pending_ticks}, visualized, drawn_points, total_points, subsampled, dropped_debug, tool }",
                Permission::Read,
            ),
            state as Handler,
        ),
        (
            m(
                "editor.procgen_panel_culling",
                "procgen_panel_culling",
                "The panel's culling of off-screen nodes and data-table rows: reads how many nodes and rows the last panel frame drew or skipped; with enabled turns culling on or off (off draws everything, for measuring).",
                &const {
                    [p(
                        "enabled",
                        "boolean",
                        false,
                        "Turn culling on (the default) or off.",
                    )]
                },
                "{ enabled, nodes_drawn, nodes_culled, rows_drawn, rows_total }",
                Permission::Read,
            ),
            panel_culling as Handler,
        ),
        (
            m(
                "editor.procgen_reload",
                "procgen_reload",
                "Check the open file for an outside change now; with take_theirs resolve a pending keep-mine/take-theirs choice (that form needs the edit_scene permission, F0012 otherwise).",
                &const {
                    [p(
                        "take_theirs",
                        "boolean",
                        false,
                        "Resolve the pending choice: true reloads the file, false keeps the edits.",
                    )]
                },
                "{ reloaded, conflict }",
                Permission::Read,
            ),
            reload as Handler,
        ),
        (
            m(
                "editor.procgen_param",
                "procgen_param",
                "Parameters tab: on an open variant set (or reset: true remove) its override; on a graph set the parameter's default, and declare, rename (every Bind follows), reorder, retype, set the range or the doc, or remove it (remove: true; bound settings fall back to its default). One call is one undo entry.",
                &const {
                    [
                        p("name", "string", true, "Parameter name."),
                        p(
                            "declare",
                            "boolean",
                            false,
                            "Declare `name` first (type `ty`, default Float), with its type's zero default.",
                        ),
                        p(
                            "rename",
                            "string",
                            false,
                            "New name; every Bind of the parameter follows.",
                        ),
                        p("index", "integer", false, "Move the parameter to this position."),
                        p(
                            "ty",
                            "string",
                            false,
                            "Type: Bool, Int, Float, Vec2, Vec3, Vec4, Quat, Transform, Name, Asset (the default converts when it can).",
                        ),
                        p(
                            "min",
                            "number",
                            false,
                            "Inclusive minimum of a number parameter; null clears.",
                        ),
                        p(
                            "max",
                            "number",
                            false,
                            "Inclusive maximum of a number parameter; null clears.",
                        ),
                        p("doc", "string", false, "The one-line doc."),
                        p(
                            "value",
                            "any",
                            false,
                            "JSON value ({\"ron\": \"...\"} for enum variants).",
                        ),
                        p(
                            "reset",
                            "boolean",
                            false,
                            "Remove the override (variant, or generator).",
                        ),
                        p(
                            "generator",
                            "integer",
                            false,
                            "Edit this generator's override instead of the document (the inspector's override column).",
                        ),
                        p(
                            "remove",
                            "boolean",
                            false,
                            "Remove the parameter's declaration (the tab's remove); every setting bound to it takes its default. Takes no other edit key; refused (procgen_read_only) on a variant.",
                        ),
                    ]
                },
                "{ name, overrides, dirty, params: [{name, ty, default, min, max, doc}] } or, with generator, { name, generator, params: [{name, ty, default, generator, value, source}] }",
                Permission::EditScene,
            ),
            param as Handler,
        ),
        (
            m(
                "editor.procgen_preview",
                "procgen_preview",
                "Bind the preview target: edits re-run this generator after the quiet period, keeping debug data. An explicit generator (or null) stays bound; follow: true returns to the default rule (the selected entity's generator, else the last one used).",
                &const {
                    [
                        p(
                            "generator",
                            "integer",
                            false,
                            "Generator entity id; null unbinds. Omitted with follow or tier only: the binding is kept.",
                        ),
                        p(
                            "follow",
                            "boolean",
                            false,
                            "true: follow the default rule (selection's generator, else last used).",
                        ),
                        p(
                            "tier",
                            "string",
                            false,
                            "Preview quality tier for flow.quality (ultra, high, medium, low, deck, mobile_high, mobile_low); null resets to high.",
                        ),
                    ]
                },
                "{ generator, follow, refresh_ticks }",
                Permission::EditScene,
            ),
            preview as Handler,
        ),
        (
            m(
                "editor.procgen_palette",
                "procgen_palette",
                "The palette: node types matching a search over id, category and doc; with from, only types with an input accepting that output pin's kinds (and the pin a drop would connect).",
                &const {
                    [
                        p("search", "string", false, "Case-insensitive search."),
                        p(
                            "from",
                            "array",
                            false,
                            "[node id, \"output pin\"] of the open graph.",
                        ),
                    ]
                },
                "{ count, types: [{ty, category, doc, pin}] }",
                Permission::Read,
            ),
            palette as Handler,
        ),
        (
            m(
                "editor.procgen_seed",
                "procgen_seed",
                "Preview bar seed actions: pin the current seed, unpin, a new seed, set one, or the same seed as another generator (one undo entry; the generator re-runs).",
                &const {
                    [
                        p("action", "string", true, "pin | unpin | new | set | same_as."),
                        p(
                            "generator",
                            "integer",
                            false,
                            "Generator (default: the preview target).",
                        ),
                        p("seed", "integer", false, "For set."),
                        p("from", "integer", false, "For same_as: the other generator."),
                    ]
                },
                "{ generator, pinned, seed }",
                Permission::EditScene,
            ),
            seed as Handler,
        ),
        (
            m(
                "editor.procgen_pin",
                "procgen_pin",
                "Show or hide a node's override pin @<setting> (a Table input that wins over the setting). Connect it with procgen.graph_edit connect to [node, \"@setting\"]; hiding removes that edge (one undo entry).",
                &const {
                    [
                        p("node", "integer", true, "Node id."),
                        p("setting", "string", true, "Setting name."),
                        p("show", "boolean", true, "Show (true) or hide (false)."),
                    ]
                },
                "{ node, shown: [setting] }",
                Permission::EditScene,
            ),
            pin as Handler,
        ),
        (
            m(
                "editor.procgen_select",
                "procgen_select",
                "Set the canvas selection (node ids).",
                &const { [p("nodes", "array", true, "Node ids.")] },
                "{ selection }",
                Permission::Read,
            ),
            select as Handler,
        ),
        (
            m(
                "editor.procgen_visualize",
                "procgen_visualize",
                "Draw a node's output in the viewport (points, boxes or axes; colored by density, an attribute or tags).",
                &const {
                    [
                        p("node", "integer", true, "Node id."),
                        p("on", "boolean", true, "Visualize or stop."),
                        p(
                            "color_by",
                            "string",
                            false,
                            "density | attr | tags (default density).",
                        ),
                        p("attr", "string", false, "Attribute for color_by attr."),
                        p("form", "string", false, "points | boxes | axes (default points)."),
                    ]
                },
                "{ visualized: [id], drawn_points, total_points, subsampled }",
                Permission::Read,
            ),
            visualize as Handler,
        ),
        (
            m(
                "editor.procgen_table",
                "procgen_table",
                "Inspect a node: its output as a data table (one pin, one item) with sorting, an `attr op value` filter, paging by 1000 rows, row selection and, for tiled generators, the cell `[tier, i, j]` whose data shows. Without node: the view the panel's data table shows (its sort, filter and page controls write the same state). A refused filter keeps the last good one.",
                &const {
                    [
                        p("node", "integer", false, "Node id (default: the inspected node)."),
                        p(
                            "toggle_sort",
                            "string",
                            false,
                            "A header click on this column: ascending, then descending, then unsorted.",
                        ),
                        p("pin", "integer", false, "Output pin index (default 0)."),
                        p("item", "integer", false, "Item index (default 0)."),
                        p(
                            "sort",
                            "string",
                            false,
                            "Column to sort by (stable); null clears.",
                        ),
                        p("descending", "boolean", false, "Sort descending."),
                        p(
                            "filter",
                            "string",
                            false,
                            "`attr op value` (<, <=, >, >=, ==, !=); empty clears.",
                        ),
                        p("page", "integer", false, "Page (1000 rows each, default 0)."),
                        p(
                            "select_row",
                            "integer",
                            false,
                            "Select the element shown in this view row.",
                        ),
                        p(
                            "select_at",
                            "array",
                            false,
                            "[x, y, z]: select the nearest point's row (within 1 m).",
                        ),
                        p(
                            "cell",
                            "array",
                            false,
                            "Tiled generators: [tier, i, j], the cell whose data shows (default: \
                             the first cell of the run); null back to that.",
                        ),
                    ]
                },
                "{ node, pins, items, kind, tags, columns, rows: [[cell]], elements: [index], total, pages, page, selected, sort: {column, descending} | null, filter }",
                Permission::Read,
            ),
            table as Handler,
        ),
        (
            m(
                "editor.procgen_isolate",
                "procgen_isolate",
                "Preview up to a node: runs it and its ancestors for the preview target without replacing the target's output; null ends isolation.",
                &const { [p("node", "integer", true, "Node id, or null.")] },
                "{ node, state: isolated | failed | off, error, nodes }",
                Permission::Read,
            ),
            isolate as Handler,
        ),
        (
            m(
                "editor.procgen_collapse",
                "procgen_collapse",
                "Move selected nodes into a new graph file and replace them with one graph.call (one undo entry).",
                &const {
                    [
                        p("nodes", "array", true, "Node ids to collapse."),
                        p("path", "string", true, "New .pgraph.ron path (must not exist)."),
                    ]
                },
                "{ call_node, new_graph, graph_hash }",
                Permission::Assets,
            ),
            collapse as Handler,
        ),
        (
            m(
                "editor.procgen_expand",
                "procgen_expand",
                "Replace a graph.call node with the called graph's nodes; a graph called from other files needs copy: true.",
                &const {
                    [
                        p("node", "integer", true, "The graph.call node."),
                        p(
                            "copy",
                            "boolean",
                            false,
                            "Expand even though other graphs call the file.",
                        ),
                    ]
                },
                "{ nodes, graph_hash }",
                Permission::Assets,
            ),
            expand as Handler,
        ),
        (
            m(
                "editor.procgen_capture",
                "procgen_capture",
                "PNG of the procedural graph panel (canvas, data table or run report) through the offscreen UI capture (heavy). With out, writes the PNG inside the project or temp directory (that form needs the edit_scene permission, F0012 otherwise).",
                &const {
                    [
                        p("panel", "string", true, "canvas | table | report."),
                        p(
                            "out",
                            "string",
                            false,
                            "PNG path inside the project or temp directory; needs the edit_scene permission (F0012 otherwise).",
                        ),
                        p("width", "integer", false, "Pixels (64-4096, default 1000)."),
                        p("height", "integer", false, "Pixels (64-4096, default 640)."),
                    ]
                },
                "{ width, height, mime, png_base64 } or { width, height, path }",
                Permission::Read,
            ),
            capture as Handler,
        ),
        (
            m(
                "editor.procgen_tool",
                "procgen_tool",
                "Scripted placement tool session (paint, box, path, area): points are projected down with the tool's hit filter; apply: true commits one undo entry, false keeps the session open. With mode and/or gesture instead of tool: arm the viewport tool mode Procedural and feed it one mouse gesture (the same path the viewport takes; paint strokes stamp in the background over the next frames).",
                &const {
                    [
                        p(
                            "tool",
                            "string",
                            false,
                            "path | area | paint | box (scripted session; required unless mode or gesture).",
                        ),
                        p(
                            "preset",
                            "string",
                            false,
                            "A graph whose flags.tool names the tool (required with tool).",
                        ),
                        p(
                            "mode",
                            "object",
                            false,
                            "Viewport mode: { tool, preset, biome?, brush? } arms it, \"off\" leaves it (cancelling an open session).",
                        ),
                        p(
                            "gesture",
                            "object",
                            false,
                            "One viewport gesture: { kind: press|move|release|backspace|close|enter|escape|brush, at?: [x,y,z] | origin + direction, radius_m?, layer?, erase? }.",
                        ),
                        p(
                            "biome",
                            "string",
                            false,
                            "Area on a biome preset: the .biome.ron the BiomeArea names (default: the preset's biome parameter).",
                        ),
                        p(
                            "target",
                            "integer",
                            false,
                            "Entity to edit (default: the selection).",
                        ),
                        p(
                            "points",
                            "array",
                            false,
                            "[[x,y,z]...]: stroke centers (paint) or control points (path, area).",
                        ),
                        p(
                            "closed",
                            "boolean",
                            false,
                            "Path: closed loop (an area is always closed).",
                        ),
                        p(
                            "move",
                            "object",
                            false,
                            "Path/area: { index, to: [x,y,z] } moves one control point (projected down).",
                        ),
                        p(
                            "remove",
                            "integer",
                            false,
                            "Path/area: index of a control point to remove.",
                        ),
                        p(
                            "clear",
                            "boolean",
                            false,
                            "Path/area: drop every control point first.",
                        ),
                        p(
                            "brush",
                            "object",
                            false,
                            "{ radius_m, spacing_m, erase, layer (0-7), density, random_yaw, seed }.",
                        ),
                        p("box", "object", false, "{ min: [x,y,z], max: [x,y,z] }."),
                        p(
                            "apply",
                            "boolean",
                            false,
                            "Commit and close the session (scripted; default false).",
                        ),
                    ]
                },
                "{ entity, created, applied, painted, added, removed, spline_points, state } (scripted) or { armed, session, outcome, cancelled, pending_strokes } (mode/gesture)",
                Permission::EditScene,
            ),
            tool as Handler,
        ),
        (
            m(
                "editor.procgen_tool_cancel",
                "procgen_tool_cancel",
                "End the open tool session, removing what it created and restoring what it edited.",
                &[],
                "{ cancelled }",
                Permission::EditScene,
            ),
            tool_cancel as Handler,
        ),
        (
            m(
                "editor.procgen_view_set",
                "procgen_view_set",
                "Procedural viewport switches (spec 66 § 4.1 and § 7, spec 67): generate around view makes the editor camera an anchor for Anchored generators, the tiles overlay outlines every cell colored by state, show_volumes draws every Shape and Spline (the selection always draws). Omitted flags keep their value; no params reads them.",
                &const {
                    [
                        p(
                            "generate_around_view",
                            "boolean",
                            false,
                            "The editor camera is an anchor.",
                        ),
                        p(
                            "tiles_overlay",
                            "boolean",
                            false,
                            "One outline per cell, colored by state (seam-risk cells magenta) with each anchor's build/release circles.",
                        ),
                        p(
                            "biome_overlay",
                            "boolean",
                            false,
                            "Outline every BiomeArea; areas with preview_alone (biome.preview) draw in a second color.",
                        ),
                        p(
                            "show_volumes",
                            "boolean",
                            false,
                            "Draw every Shape volume and Spline curve, not only the selected ones.",
                        ),
                        p(
                            "biome_tint",
                            "boolean",
                            false,
                            "Biome overlay: the claim raster, each cell crossed in its owning biome's color (alpha by the owner's weight).",
                        ),
                        p(
                            "biome_bands",
                            "boolean",
                            false,
                            "Biome overlay: outline the claim cells inside a blend band (a second claim shares them).",
                        ),
                        p(
                            "biome_footprints",
                            "boolean",
                            false,
                            "Biome overlay: core and spread circles of the preview run's placed points.",
                        ),
                        p(
                            "biome_filtered",
                            "boolean",
                            false,
                            "Biome overlay: the candidates each entry filter removed in the preview run, dim (editor.procgen_biome_overlay `at` names the filter).",
                        ),
                    ]
                },
                "{ generate_around_view, tiles_overlay, show_volumes, biome_overlay, biome_tint, biome_bands, biome_footprints, biome_filtered, anchors }",
                Permission::EditScene,
            ),
            view_set as Handler,
        ),
        (
            m(
                "editor.procgen_biome_overlay",
                "procgen_biome_overlay",
                "Read the biome debug overlay (spec 65 § 7): claim cells per biome, blend-band cells, footprints and the candidates each entry filter removed in the preview run, counted whether or not the views draw; `at` answers the filter view's hover (the nearest removed candidate and its filter).",
                &const {
                    [
                        p(
                            "at",
                            "array",
                            false,
                            "[x, z]: the nearest removed candidate within radius_m is returned as `hover`.",
                        ),
                        p(
                            "radius_m",
                            "number",
                            false,
                            "Hover radius, meters (default 1, positive).",
                        ),
                    ]
                },
                "{ views, claim_cells, band_cells, draped_cells, biomes, footprints, footprints_drawn, filtered, filters, hover }",
                Permission::Read,
            ),
            biome_overlay as Handler,
        ),
        (
            m(
                "editor.plant_preview",
                "plant_preview",
                "The plant preview (spec 68 § 8): runs a Plant graph in memory for the six seeds of the variants strip (seeds .. seeds + 5) and the plant's seed, as the panel's asset viewport shows them; seed sets the plant's seed (a click on the strip). Nothing is written to the level. Heavy: up to seven graph runs.",
                &const {
                    [
                        p("asset", "string", true, "A Plant graph (.pgraph.ron)."),
                        p(
                            "seeds",
                            "integer",
                            false,
                            "The strip's first seed (default: unchanged, 0 at first).",
                        ),
                        p(
                            "seed",
                            "integer",
                            false,
                            "Set the plant's seed (default: unchanged; the strip's first seed when outside it).",
                        ),
                        p(
                            "node",
                            "integer",
                            false,
                            "Show this node's skeleton or mesh (default: the last node with plant data).",
                        ),
                        p(
                            "lit",
                            "boolean",
                            false,
                            "Render the lit mesh thumbnails the panel shows (needs a render device; a skeleton node has none). Default false: report the ones already drawn.",
                        ),
                    ]
                },
                "{ asset, seed, node, seeds: [6], variants: [{ seed, node, plants, nodes, height_m, crown_width_m, triangles, segments, error, lit: { width, height, coverage, mean_rgb } | null }], shown, builds, lit_renders }",
                Permission::Read,
            ),
            plant_preview as Handler,
        ),
        (
            m(
                "editor.procgen_widgets",
                "procgen_widgets",
                "The node settings panel's typed widgets (spec 67 § 2.1): per setting its widget (number, integer, toggle, vec3, range2, vec3_range, enum, curve, asset, name, name_list, text), value, range, enum options, curve keys, list items, asset picker choices and the parameter it is bound to. With check, validates a value as the widget does before writing (never writes).",
                &const {
                    [
                        p("node", "integer", true, "Node id of the open graph."),
                        p(
                            "search",
                            "string",
                            false,
                            "Asset picker search (case-insensitive substring of the project path).",
                        ),
                        p(
                            "check",
                            "object",
                            false,
                            "{ setting, value }: check this value ({\"ron\": \"...\"} for RON).",
                        ),
                    ]
                },
                "{ node, ty, settings: [{name, widget, value, bound, min, max, options, points, items, choices, doc}], check: {ok, reason, message} }",
                Permission::Read,
            ),
            widgets as Handler,
        ),
        (
            m(
                "editor.procgen_badges",
                "procgen_badges",
                "The canvas's node badges and pin colors (spec 67 § 2.2): bypassed, visualized, inspected, tier (the preview target's tier cell size), cache_hit, warnings (count) and error per node; each pin's colors from the theme's data_kinds (several for a striped multi-kind pin).",
                &const { [p("nodes", "array", false, "Node ids (default: every node).")] },
                "{ nodes: [{id, badges: [name], warnings, tier: {index, cell_m} | null, cache_hit, error: {reason, message} | null, pins: {inputs: [{name, colors}], outputs: [{name, colors}]}}] }",
                Permission::Read,
            ),
            badges as Handler,
        ),
        (
            m(
                "editor.procgen_promote",
                "procgen_promote",
                "Promote a node setting to a graph parameter with a name (spec 67 § 2.4, the bind toggle's name prompt): declares the parameter (typed after the setting, its current value as default) and binds the setting, one undo entry; existing: true binds to a parameter that exists.",
                &const {
                    [
                        p("node", "integer", true, "Node id."),
                        p("setting", "string", true, "Setting name."),
                        p(
                            "name",
                            "string",
                            false,
                            "Parameter name (default: the setting's name, made unique).",
                        ),
                        p(
                            "existing",
                            "boolean",
                            false,
                            "Bind to the existing parameter `name` instead of declaring one.",
                        ),
                    ]
                },
                "{ param, created, node, setting }",
                Permission::EditScene,
            ),
            promote as Handler,
        ),
    ]
}

fn plant_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use super::plant_preview::{self as pp, PlantPreview};
    super::install_if_missing(world);
    let asset = str_param(params, "asset", "procgen_unknown_graph")?.to_string();
    let u32_of = |name: &str| -> Result<Option<u32>, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|x| u32::try_from(x).ok())
                .map(Some)
                .ok_or_else(|| bad("procgen_param_type", format!("`{name}` is a u32"))),
        }
    };
    let first = u32_of("seeds")?;
    let seed = u32_of("seed")?;
    let node = u32_of("node")?;
    pp::set_asset(world, &asset, first, node).map_err(rpc)?;
    if let Some(s) = seed {
        pp::set_seed(world, s);
    }
    pp::build(world, None);
    let lit = match params.get("lit") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad("procgen_param_type", "`lit` is a boolean"))?,
    };
    if lit {
        // Lit thumbnails need a render device (`editor_headless` on a headless editor without one).
        crate::agent_ext::viewport_device(world)?;
        pp::render_lit(world, None);
    }
    let p = world.resource::<PlantPreview>();
    let lit_json = |seed: u32| {
        p.lit.get(&seed).map(|t| {
            let (coverage, mean_rgb) = t.coverage();
            json!({
                "width": t.image.size[0],
                "height": t.image.size[1],
                "coverage": coverage,
                "mean_rgb": mean_rgb,
            })
        })
    };
    let variant = |v: &pp::PlantVariant| {
        json!({
            "lit": lit_json(v.seed),
            "seed": v.seed,
            "node": v.node,
            "plants": v.plants,
            "nodes": v.nodes,
            "height_m": v.height_m,
            "crown_width_m": v.crown_width_m,
            "triangles": v.triangles,
            "segments": v.segments.len(),
            "error": v.error,
        })
    };
    let seeds = p.strip_seeds();
    let variants: Vec<Value> = seeds
        .iter()
        .filter_map(|s| p.variants.get(s))
        .map(variant)
        .collect();
    Ok(json!({
        "asset": asset,
        "seed": p.seed,
        "node": p.node,
        "seeds": seeds,
        "variants": variants,
        "shown": p.shown().map(variant),
        "builds": p.builds,
        "lit_renders": p.lit_renders,
    }))
}

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn str_param<'a>(params: &'a Value, name: &str, why: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(why, format!("`{name}` (a string) is required")))
}

fn u32_param(params: &Value, name: &str) -> Option<u32> {
    params
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

fn errors_json(errors: &[regia_procgen::GraphError]) -> Value {
    Value::Array(
        errors
            .iter()
            .map(|e| json!({ "node": e.node, "reason": e.code, "message": e.message }))
            .collect(),
    )
}

fn doc_summary(world: &World) -> Value {
    let Some(d) = world.get_resource::<ProcGraphDoc>() else {
        return json!({});
    };
    let registry = doc::registry(world);
    json!({
        "path": d.path,
        "graph_hash": hex(&d.graph.graph_hash(&registry)),
        "errors": errors_json(&d.errors),
        "variant": d.variant.as_ref().map(|v| v.path.clone()),
    })
}

fn open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    if let Some(g) = entity_param(params, "generator")? {
        super::inspector::open_generator_graph(world, g).map_err(rpc)?;
    } else if let Some(node) = u32_param(params, "enter") {
        doc::enter_call(world, node).map_err(rpc)?;
    } else if params.get("back").and_then(Value::as_bool).unwrap_or(false) {
        doc::back(world).map_err(rpc)?;
    } else {
        let path = str_param(params, "path", "procgen_unknown_graph")?.to_string();
        doc::open(world, &path).map_err(rpc)?;
        world.resource_mut::<ProcGraphDoc>().breadcrumb.clear();
    }
    let mut out = doc_summary(world);
    out["breadcrumb"] = json!(world.resource::<ProcGraphDoc>().breadcrumb);
    Ok(out)
}

/// The editing form of a method registered `read` (Make starter, resolving a reload, a capture
/// written to disk): refused with `F0012` when the permission profile lacks `edit_scene`, as the
/// dispatcher refuses an `edit_scene` method (spec 67 § 6, ADR-5646). A host without the agent's
/// permission resource is not restricted.
fn require_edit_scene(world: &World) -> Result<(), RpcError> {
    match world.get_resource::<regia_agent::AgentPermissions>() {
        Some(p) if !p.allows(Permission::EditScene) => Err(p.denied_error(Permission::EditScene)),
        _ => Ok(()),
    }
}

fn starters(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    if let Some(flag) = params.get("starter").and_then(Value::as_bool) {
        require_edit_scene(world)?;
        require_open(world)?;
        doc::set_starter(world, flag).map_err(rpc)?;
    }
    let enabled = world
        .get_resource::<debug_view::ProcEditorPrefs>()
        .is_none_or(|p| p.starter_picker);
    let list: Vec<Value> = if enabled {
        doc::starters(world)
            .into_iter()
            .map(|s| json!({ "path": s.path, "tool": s.tool, "domain": s.domain, "description": s.description }))
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({ "enabled": enabled, "starters": list }))
}

fn palette(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    let search = params.get("search").and_then(Value::as_str).unwrap_or("");
    let from = match params.get("from").and_then(Value::as_array) {
        Some(a) => {
            let node = a
                .first()
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .ok_or_else(|| bad("procgen_unknown_node", "`from` is [node id, \"pin\"]"))?;
            let pin = a.get(1).and_then(Value::as_str).unwrap_or("out");
            Some(doc::output_kinds(world, node, pin).ok_or_else(|| {
                bad(
                    "procgen_unknown_node",
                    format!("node {node} has no output pin `{pin}`"),
                )
            })?)
        }
        None => None,
    };
    let registry = doc::registry(world);
    let types: Vec<Value> = doc::palette(&registry, search, from)
        .into_iter()
        .map(|e| json!({ "ty": e.ty, "category": e.category, "doc": e.doc, "pin": e.pin }))
        .collect();
    Ok(json!({ "count": types.len(), "types": types }))
}

fn seed(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    let target = match entity_param(params, "generator")? {
        Some(e) => e,
        None => world
            .resource::<ProcPreview>()
            .target
            .ok_or_else(|| bad("procgen_no_preview", "pass `generator` or bind a preview target"))?,
    };
    if world.get::<regia_procgen::world::ProcGenerator>(target).is_none() {
        return Err(bad(
            "procgen_not_generator",
            format!("entity {} has no ProcGenerator", target.to_bits()),
        ));
    }
    let action_name = str_param(params, "action", "procgen_bad_seed")?;
    let action = match action_name {
        "pin" => doc::SeedAction::Pin,
        "unpin" => doc::SeedAction::Unpin,
        "new" => doc::SeedAction::New,
        "set" => doc::SeedAction::Set(
            u32_param(params, "seed")
                .ok_or_else(|| bad("procgen_bad_seed", "`seed` (u32) is required for set"))?,
        ),
        "same_as" => {
            let from = entity_param(params, "from")?
                .filter(|e| world.get::<regia_procgen::world::ProcGenerator>(*e).is_some())
                .ok_or_else(|| bad("procgen_not_generator", "`from` is another generator"))?;
            doc::SeedAction::SameAs(from)
        }
        other => {
            return Err(bad(
                "procgen_bad_seed",
                format!("unknown action `{other}` (pin | unpin | new | set | same_as)"),
            ));
        }
    };
    let (pinned, effective) = doc::apply_seed_action(world, target, action).map_err(rpc)?;
    Ok(json!({ "generator": target.to_bits(), "pinned": pinned, "seed": effective }))
}

fn new(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    let path = str_param(params, "path", "procgen_bad_path")?.to_string();
    let starter = params.get("starter").and_then(Value::as_str).map(str::to_string);
    doc::new_graph(world, &path, starter.as_deref()).map_err(rpc)?;
    Ok(doc_summary(world))
}

fn state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    let d = world.resource::<ProcGraphDoc>();
    let preview = world.resource::<ProcPreview>();
    let target = preview.target;
    let st = target.and_then(|e| world.get::<regia_procgen::world::ProcGenState>(e));
    let session = world
        .get_resource::<tools::ProcToolSession>()
        .and_then(|s| s.0.as_ref())
        .map(|s| json!({ "tool": s.kind.name(), "preset": s.preset, "entity": s.target.to_bits(), "created": s.created }));
    Ok(json!({
        "path": d.path,
        "dirty": d.dirty(),
        "read_only": d.read_only(),
        "conflict": d.conflict.is_some(),
        "selection": d.selection.iter().collect::<Vec<_>>(),
        "errors": errors_json(&d.errors),
        "revision": d.revision,
        "breadcrumb": d.breadcrumb,
        "toast": d.toast,
        "preview": {
            "generator": target.map(Entity::to_bits),
            "state": st.map(|s| s.state.name()),
            "run_id": st.map(|s| s.run_id),
            "output_hash": st.and_then(|s| s.output_hash.as_ref().map(hex)),
            "runs": preview.runs,
            "cancelled": preview.cancelled,
            "pending_ticks": preview.pending,
        },
        "visualized": preview.visualize.keys().collect::<Vec<_>>(),
        "drawn_points": preview.drawn.len(),
        "total_points": preview.total_points,
        "subsampled": preview.subsampled,
        "dropped_debug": preview.dropped_debug,
        "isolated": preview.isolate.as_ref().map(|i| i.node),
        "undo_depth": world.get_resource::<crate::command::UndoStack>().map_or(0, |u| u.undo_depth()),
        "tool": session,
    }))
}

/// `editor.procgen_panel_culling`.
fn panel_culling(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(enabled) = params.get("enabled") {
        let on = enabled
            .as_bool()
            .ok_or_else(|| bad("procgen_bad_param", "`enabled` is a boolean"))?;
        super::panel::set_culling(world, on);
    }
    let (rows_drawn, rows_total) = super::panel::table_rows(world);
    Ok(json!({
        "enabled": super::panel::culling(world),
        "nodes_drawn": super::panel::nodes_drawn(world),
        "nodes_culled": super::panel::nodes_culled(world),
        "rows_drawn": rows_drawn,
        "rows_total": rows_total,
    }))
}

fn reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    if let Some(take) = params.get("take_theirs").and_then(Value::as_bool) {
        require_edit_scene(world)?;
        doc::resolve_conflict(world, take).map_err(rpc)?;
        return Ok(json!({ "reloaded": take, "conflict": false }));
    }
    let reloaded = doc::check_external(world);
    let conflict = world.resource::<ProcGraphDoc>().conflict.is_some();
    Ok(json!({ "reloaded": reloaded, "conflict": conflict }))
}

fn entity_param(params: &Value, name: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let bits = v
                .as_u64()
                .ok_or_else(|| bad("procgen_not_generator", format!("`{name}` is an entity id")))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| bad("procgen_not_generator", format!("{bits} is not an entity id")))
        }
    }
}

fn json_setting(v: &Value) -> Result<regia_procgen::SettingValue, String> {
    use regia_procgen::SettingValue as S;
    Ok(match v {
        Value::Null => S::Ident("None".into()),
        Value::Bool(b) => S::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => S::Int(i),
            None => S::Float(n.as_f64().ok_or("number out of range")?),
        },
        Value::String(t) => S::Str(t.clone()),
        Value::Array(a) => S::List(a.iter().map(json_setting).collect::<Result<_, _>>()?),
        Value::Object(o) => {
            if let (1, Some(Value::String(r))) = (o.len(), o.get("ron")) {
                return S::parse(r).map_err(|e| e.to_string());
            }
            S::Record(
                None,
                o.iter()
                    .map(|(k, x)| json_setting(x).map(|v| (k.clone(), v)))
                    .collect::<Result<_, _>>()?,
            )
        }
    })
}

fn param(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "name", "procgen_unknown_param")?.to_string();
    let reset = params.get("reset").and_then(Value::as_bool).unwrap_or(false);
    // `remove` is checked before any branch: a non-boolean is refused, as the data asset's
    // `discard` is, and it never combines with `generator` (rev-claude-58 parity finding 2,
    // ADR-7495).
    let remove = match params.get("remove") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad("procgen_param_value", "`remove` is a boolean"))?,
    };
    if remove && params.get("generator").is_some_and(|g| !g.is_null()) {
        return Err(bad(
            "procgen_param_value",
            "`remove: true` removes the graph's declaration and takes no `generator` (reset: true clears an override)",
        ));
    }
    if let Some(g) = entity_param(params, "generator")? {
        let value = if reset {
            None
        } else {
            let v = params
                .get("value")
                .ok_or_else(|| bad("procgen_param_value", "`value` (or reset: true) is required"))?;
            Some(
                json_setting(v)
                    .map_err(|m| bad("procgen_param_value", m))?
                    .write(),
            )
        };
        super::inspector::set_generator_param(world, g, &name, value).map_err(rpc)?;
        let rows = super::inspector::param_rows(world, g).map_err(rpc)?;
        return Ok(json!({
            "name": name,
            "generator": g.to_bits(),
            "params": rows.iter().map(|r| json!({
                "name": r.name, "ty": r.ty, "default": r.default, "generator": r.generator,
                "value": r.effective.0, "source": r.effective.1,
            })).collect::<Vec<_>>(),
        }));
    }
    require_open(world)?;
    let (actions, final_name) = param_actions(world, &name, params, reset)?;
    if actions.is_empty() {
        let value = if reset {
            None
        } else {
            let v = params
                .get("value")
                .ok_or_else(|| bad("procgen_param_value", "`value` (or reset: true) is required"))?;
            Some(json_setting(v).map_err(|m| bad("procgen_param_value", m))?)
        };
        doc::edit(
            world,
            &format!("set parameter {name}"),
            Some(format!("param:{name}")),
            |c| doc::set_param_value(c, &name, value),
        )
        .map_err(rpc)?;
    } else {
        super::params_tab::apply_param_actions(world, &format!("edit parameter {name}"), &actions)
            .map_err(rpc)?;
    }
    let name = final_name;
    let d = world.resource::<ProcGraphDoc>();
    let overrides: serde_json::Map<String, Value> = d
        .variant
        .as_ref()
        .map(|v| {
            v.variant
                .overrides
                .iter()
                .map(|(k, x)| (k.clone(), json!(x.write())))
                .collect()
        })
        .unwrap_or_default();
    let rows: Vec<Value> = d
        .graph
        .params
        .iter()
        .map(|p| json!({ "name": p.name, "ty": p.ty.name(), "default": p.default.write(), "min": p.min, "max": p.max, "doc": p.doc }))
        .collect();
    Ok(json!({ "name": name, "overrides": overrides, "dirty": d.dirty(), "params": rows }))
}

/// The Parameters tab actions an `editor.procgen_param` call asks for (declare, type, value or
/// reset, range, doc, index, rename, in that order), and the parameter's name after them.
fn param_actions(
    world: &World,
    name: &str,
    params: &Value,
    reset: bool,
) -> Result<(Vec<super::params_tab::ParamAction>, String), RpcError> {
    use super::params_tab::ParamAction as A;
    if params.get("remove").and_then(Value::as_bool).unwrap_or(false) {
        // The Parameters tab's "remove": drop the declaration and unbind every bound setting to
        // the parameter's default. It is the whole edit.
        let others = ["declare", "rename", "index", "ty", "min", "max", "doc", "value"];
        if reset || others.iter().any(|k| params.get(*k).is_some()) {
            return Err(bad(
                "procgen_param_value",
                "`remove: true` takes only `name` (no other edit in the same call)",
            ));
        }
        return Ok((
            vec![A::Remove {
                name: name.to_string(),
            }],
            name.to_string(),
        ));
    }
    let keys = ["declare", "rename", "index", "ty", "min", "max", "doc"];
    if !keys.iter().any(|k| params.get(*k).is_some()) {
        return Ok((Vec::new(), name.to_string()));
    }
    let ty = match params.get("ty") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let t = v.as_str().unwrap_or("");
            Some(regia_procgen::AttrType::from_name(t).ok_or_else(|| {
                bad(
                    "procgen_param_type",
                    format!("unknown type `{t}` (Bool, Int, Float, Vec2, Vec3, Vec4, Quat, Transform, Name, Asset)"),
                )
            })?)
        }
    };
    let mut out = Vec::new();
    let declare = params.get("declare").and_then(Value::as_bool).unwrap_or(false);
    if declare {
        out.push(A::Declare {
            name: name.to_string(),
            ty: ty.unwrap_or(regia_procgen::AttrType::Float),
        });
    } else if let Some(ty) = ty {
        out.push(A::SetType {
            name: name.to_string(),
            ty,
        });
    }
    if reset {
        out.push(A::SetDefault {
            name: name.to_string(),
            value: None,
        });
    } else if let Some(v) = params.get("value") {
        out.push(A::SetDefault {
            name: name.to_string(),
            value: Some(json_setting(v).map_err(|m| bad("procgen_param_value", m))?),
        });
    }
    if params.get("min").is_some() || params.get("max").is_some() {
        let current = world
            .get_resource::<ProcGraphDoc>()
            .and_then(|d| d.graph.param(name).map(|p| (p.min, p.max)))
            .unwrap_or((None, None));
        let bound = |key: &str, keep: Option<f64>| -> Result<Option<f64>, RpcError> {
            match params.get(key) {
                None => Ok(keep),
                Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_f64()
                    .map(Some)
                    .ok_or_else(|| bad("procgen_param_range", format!("`{key}` is a number or null"))),
            }
        };
        out.push(A::SetRange {
            name: name.to_string(),
            min: bound("min", current.0)?,
            max: bound("max", current.1)?,
        });
    }
    if let Some(d) = params.get("doc") {
        out.push(A::SetDoc {
            name: name.to_string(),
            doc: d
                .as_str()
                .ok_or_else(|| bad("procgen_param_value", "`doc` is a string"))?
                .to_string(),
        });
    }
    if let Some(i) = params.get("index") {
        out.push(A::Move {
            name: name.to_string(),
            to: i
                .as_u64()
                .and_then(|x| usize::try_from(x).ok())
                .ok_or_else(|| bad("procgen_param_value", "`index` is a non-negative integer"))?,
        });
    }
    let mut final_name = name.to_string();
    if let Some(to) = params.get("rename") {
        let to = to
            .as_str()
            .ok_or_else(|| bad("procgen_param_name", "`rename` is a string"))?;
        out.push(A::Rename {
            from: name.to_string(),
            to: to.to_string(),
        });
        final_name = to.to_string();
    }
    Ok((out, final_name))
}

fn preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    let target = entity_param(params, "generator")?;
    let tier = match params.get("tier") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let name = v.as_str().unwrap_or("");
            Some(Some(
                regia_render::quality::QualityTier::from_name(name)
                    .ok_or_else(|| bad("procgen_bad_tier", format!("unknown quality tier `{name}`")))?,
            ))
        }
    };
    let follow = params.get("follow").and_then(Value::as_bool).unwrap_or(false);
    if follow || params.get("generator").is_some() {
        debug_view::choose_preview(world, target, follow).map_err(rpc)?;
    }
    if let Some(t) = tier {
        debug_view::set_tier(world, t);
    }
    let ticks = world
        .get_resource::<debug_view::ProcEditorPrefs>()
        .map_or(9, debug_view::ProcEditorPrefs::refresh_ticks);
    let (bound, pinned) = world
        .get_resource::<ProcPreview>()
        .map_or((None, false), |p| (p.target, p.pinned_target));
    Ok(json!({
        "generator": bound.map(Entity::to_bits),
        "follow": !pinned,
        "refresh_ticks": ticks,
    }))
}

fn node_ids(params: &Value, name: &str) -> Result<BTreeSet<u32>, RpcError> {
    params
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| bad("procgen_unknown_node", format!("`{name}` is a list of node ids")))?
        .iter()
        .map(|v| {
            v.as_u64()
                .and_then(|x| u32::try_from(x).ok())
                .ok_or_else(|| bad("procgen_unknown_node", "node ids are integers"))
        })
        .collect()
}

fn require_open(world: &World) -> Result<(), RpcError> {
    if world
        .get_resource::<ProcGraphDoc>()
        .is_some_and(|d| d.path.is_some())
    {
        Ok(())
    } else {
        Err(bad(
            "procgen_not_open",
            "no procedural graph is open (editor.procgen_open)",
        ))
    }
}

fn pin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = u32_param(params, "node").ok_or_else(|| bad("procgen_unknown_node", "`node` is required"))?;
    let setting = str_param(params, "setting", "unknown_setting")?.to_string();
    let show = params.get("show").and_then(Value::as_bool).unwrap_or(true);
    doc::show_pin(world, node, &setting, show).map_err(rpc)?;
    let d = world.resource::<ProcGraphDoc>();
    let shown: Vec<&String> = d
        .shown_pins
        .iter()
        .filter(|(n, _)| *n == node)
        .map(|(_, s)| s)
        .collect();
    Ok(json!({ "node": node, "shown": shown }))
}

fn select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let ids = node_ids(params, "nodes")?;
    let mut d = world.resource_mut::<ProcGraphDoc>();
    if let Some(missing) = ids.iter().find(|id| !d.graph.nodes.contains_key(id)) {
        return Err(bad("procgen_unknown_node", format!("no node {missing}")));
    }
    d.selection = ids;
    Ok(json!({ "selection": d.selection.iter().collect::<Vec<_>>() }))
}

fn visualize(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = u32_param(params, "node").ok_or_else(|| bad("procgen_unknown_node", "`node` is required"))?;
    if !world.resource::<ProcGraphDoc>().graph.nodes.contains_key(&node) {
        return Err(bad("procgen_unknown_node", format!("no node {node}")));
    }
    let on = params.get("on").and_then(Value::as_bool).unwrap_or(true);
    let color_by = match params
        .get("color_by")
        .and_then(Value::as_str)
        .unwrap_or("density")
    {
        "density" => ColorBy::Density,
        "tags" => ColorBy::Tags,
        "attr" => ColorBy::Attr(str_param(params, "attr", "procgen_bad_visualize")?.to_string()),
        other => {
            return Err(bad(
                "procgen_bad_visualize",
                format!("unknown color_by `{other}`"),
            ));
        }
    };
    let form = match params.get("form").and_then(Value::as_str).unwrap_or("points") {
        "points" => VisForm::Points,
        "boxes" => VisForm::Boxes,
        "axes" => VisForm::Axes,
        other => return Err(bad("procgen_bad_visualize", format!("unknown form `{other}`"))),
    };
    let index = world.resource::<ProcPreview>().visualize.len();
    let style = on.then(|| VisualizeStyle {
        color_by,
        form,
        color: debug_view::categorical(index),
    });
    let ids = debug_view::visualize(world, node, style);
    let preview = world.resource::<ProcPreview>();
    Ok(json!({
        "visualized": ids,
        "drawn_points": preview.drawn.len(),
        "total_points": preview.total_points,
        "subsampled": preview.subsampled,
    }))
}

fn table(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = match params.get("node") {
        None | Some(Value::Null) => world
            .get_resource::<ProcInspect>()
            .and_then(|i| i.node)
            .ok_or_else(|| bad("procgen_no_inspect", "no node is inspected: pass `node`"))?,
        Some(_) => {
            u32_param(params, "node").ok_or_else(|| bad("procgen_unknown_node", "`node` is a node id"))?
        }
    };
    // A tiled generator's Inspect shows one cell's data (spec 67 § 3). The pick is read and
    // checked before the view changes, so a refused `cell` leaves the whole view as it was.
    let bad_cell = || {
        bad(
            "procgen_bad_cell",
            "`cell` is [tier, i, j] (three integers) or null",
        )
    };
    let cell_pick: Option<Option<(u32, i32, i32)>> = match params.get("cell") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Array(list)) if list.len() == 3 => {
            let part = |v: &Value, span: i64| v.as_i64().filter(|n| (-span..span).contains(n));
            let want = match (
                part(&list[0], 256).and_then(|n| u32::try_from(n).ok()),
                part(&list[1], 1 << 30).and_then(|n| i32::try_from(n).ok()),
                part(&list[2], 1 << 30).and_then(|n| i32::try_from(n).ok()),
            ) {
                (Some(tier), Some(i), Some(j)) => (tier, i, j),
                _ => return Err(bad_cell()),
            };
            if !inspect::inspect_cells(world).contains(&want) {
                return Err(rpc(DocError::new(
                    "procgen_unknown_cell",
                    format!(
                        "the tiled run has no cell ({}, {}, {}); `cells` lists them",
                        want.0, want.1, want.2
                    ),
                )));
            }
            Some(Some(want))
        }
        Some(_) => return Err(bad_cell()),
    };
    // A refused filter or sort column leaves the whole view as it was.
    let before = world.get_resource::<ProcInspect>().cloned();
    let restore = |world: &mut World, e: DocError| -> RpcError {
        match before.clone() {
            Some(b) => world.insert_resource(b),
            None => {
                world.remove_resource::<ProcInspect>();
            }
        }
        rpc(e)
    };
    {
        let mut ins = world.get_resource_or_insert_with(ProcInspect::default);
        if ins.node != Some(node) {
            *ins = ProcInspect {
                node: Some(node),
                ..ProcInspect::default()
            };
        }
        if let Some(pin) = params.get("pin").and_then(Value::as_u64) {
            ins.pin = usize::try_from(pin).unwrap_or(0);
        }
        if let Some(item) = params.get("item").and_then(Value::as_u64) {
            ins.item = usize::try_from(item).unwrap_or(0);
        }
        if let Some(pick) = cell_pick {
            ins.cell = pick;
        }
        match params.get("sort") {
            Some(Value::String(s)) => {
                let desc = params.get("descending").and_then(Value::as_bool).unwrap_or(false);
                ins.sort = Some((s.clone(), desc));
            }
            Some(Value::Null) => ins.sort = None,
            _ => {}
        }
        if let Some(column) = params.get("toggle_sort").and_then(Value::as_str) {
            ins.sort = inspect::next_sort(ins.sort.as_ref(), column);
        }
        if let Some(page) = params.get("page").and_then(Value::as_u64) {
            ins.page = usize::try_from(page).unwrap_or(0);
        }
    }
    if let Some(f) = params.get("filter").and_then(Value::as_str) {
        let f = f.to_string();
        let page = world.resource::<ProcInspect>().page;
        if let Err(e) = inspect::apply_table_control(world, inspect::TableControl::Filter(f)) {
            return Err(restore(world, e));
        }
        if params.get("page").is_some() {
            world.resource_mut::<ProcInspect>().page = page;
        }
    }
    if let Err(e) = inspect::build_table(world)
        && matches!(
            e.code.as_str(),
            "procgen_filter_invalid" | "procgen_unknown_column"
        )
    {
        return Err(restore(world, e));
    }
    if let Some(row) = params.get("select_row").and_then(Value::as_u64) {
        inspect::select_row(world, usize::try_from(row).unwrap_or(usize::MAX)).map_err(rpc)?;
    }
    if let Some(at) = params.get("select_at").and_then(Value::as_array) {
        let c = |i: usize| at.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        inspect::select_at(world, [c(0), c(1), c(2)], 1.0).map_err(rpc)?;
    }
    let t = inspect::build_table(world).map_err(rpc)?;
    let ins = world.resource::<ProcInspect>();
    let page = ins.page.min(t.pages() - 1);
    let elements = t.page(page);
    let rows: Vec<Value> = elements
        .iter()
        .map(|e| {
            Value::Array(
                t.rows[*e]
                    .iter()
                    .map(|c| match c {
                        inspect::Cell::Num(v) => json!(v),
                        inspect::Cell::Text(s) => json!(s),
                    })
                    .collect(),
            )
        })
        .collect();
    let cell = t.cell.map(|(tier, i, j)| json!([tier, i, j]));
    Ok(json!({
        "node": node,
        "pins": t.pins,
        "items": t.items,
        "kind": t.kind,
        "tags": t.tags,
        "columns": t.columns,
        "rows": rows,
        "elements": elements,
        "total": t.order.len(),
        "pages": t.pages(),
        "page": page,
        "cell": cell,
        "cells": inspect::inspect_cells(world),
        "selected": ins.selected,
        "sort": ins.sort.as_ref().map(|(c, d)| json!({ "column": c, "descending": d })),
        "filter": ins.filter,
    }))
}

fn isolate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = match params.get("node") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|x| u32::try_from(x).ok())
                .ok_or_else(|| bad("procgen_unknown_node", "`node` is a node id or null"))?,
        ),
    };
    let iso = debug_view::isolate(world, node).map_err(rpc)?;
    Ok(match iso {
        None => json!({ "node": null, "state": "off" }),
        Some(i) => json!({
            "node": i.node,
            "state": if i.error.is_some() { "failed" } else { "isolated" },
            "error": i.error.as_ref().map(|(n, c, m)| json!({ "node": n, "reason": c, "message": m })),
            "nodes": i.debug.keys().collect::<Vec<_>>(),
        }),
    })
}

fn collapse(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let ids = node_ids(params, "nodes")?;
    let path = str_param(params, "path", "procgen_bad_path")?.to_string();
    if doc::source_text(world, &path).is_some() {
        return Err(bad("procgen_file_exists", format!("`{path}` already exists")));
    }
    let out = doc::collapse_selection(world, &ids, &path).map_err(rpc)?;
    let summary = doc_summary(world);
    Ok(json!({ "call_node": out, "new_graph": path, "graph_hash": summary["graph_hash"] }))
}

fn expand(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = u32_param(params, "node").ok_or_else(|| bad("procgen_unknown_node", "`node` is required"))?;
    let copy = params.get("copy").and_then(Value::as_bool).unwrap_or(false);
    let nodes = doc::expand_call(world, node, copy).map_err(rpc)?;
    let summary = doc_summary(world);
    Ok(json!({ "nodes": nodes, "graph_hash": summary["graph_hash"] }))
}

fn capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "panel", "procgen_bad_panel")?;
    let view = super::panel::CaptureView::parse(name).ok_or_else(|| {
        bad(
            "procgen_bad_panel",
            format!("unknown panel `{name}` (canvas | table | report)"),
        )
    })?;
    let width = crate::agent_ext::capture_dim(params, "width", 1000);
    let height = crate::agent_ext::capture_dim(params, "height", 640);
    // Spec 67 § 6: writing to `out` needs `edit_scene`. Checked with the path before the
    // device is fetched, so a refused or bad `out` costs no capture.
    let out = match params.get("out").and_then(Value::as_str) {
        Some(out) => {
            require_edit_scene(world)?;
            Some(confined_out(world, out)?)
        }
        None => None,
    };
    let device = crate::agent_ext::viewport_device(world)?;
    let image =
        super::panel::capture_panel(&device, world, view, [width, height]).map_err(RpcError::internal)?;
    if let Some(path) = out {
        let bytes = crate::ui_capture::png_bytes(&image).map_err(RpcError::internal)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| RpcError::internal(e.to_string()))?;
        }
        // Atomic write with the Windows rename-over-open fallback and temp cleanup
        // (sprint audit g, finding 1).
        crate::atomic_write::write_atomic(&path, &bytes).map_err(RpcError::internal)?;
        return Ok(
            json!({ "width": image.width(), "height": image.height(), "path": path.display().to_string() }),
        );
    }
    let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
    Ok(json!({ "width": image.width(), "height": image.height(), "mime": "image/png", "png_base64": png }))
}

/// `out` under the agent's allowed roots (project, system temp directory, working directory,
/// `output_roots`; the `render.out_start` rule), no `..`, no symlink leading out (ADR-1816: the old
/// text check let `C:x`, `\\x` and symlinks through).
fn confined_out(world: &World, out: &str) -> Result<std::path::PathBuf, RpcError> {
    crate::agent_ext::allowed_request_path(world, "out", out).map_err(|e| bad("procgen_bad_path", e.message))
}

fn vec3(v: &Value) -> Option<Vec3> {
    let a = v.as_array()?;
    Some(Vec3::new(
        a.first()?.as_f64()? as f32,
        a.get(1)?.as_f64()? as f32,
        a.get(2)?.as_f64()? as f32,
    ))
}

/// The brush in a `brush` object (defaults for missing fields).
fn brush_param(b: &Value) -> Result<Brush, RpcError> {
    let d = Brush::default();
    let f = |k: &str, dv: f32| b.get(k).and_then(Value::as_f64).map_or(dv, |x| x as f32);
    // `layer` is an integer 0-7; a negative, fractional or non-number value is an error, not
    // layer 0 (zai audit M4).
    let layer = match b.get("layer") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .and_then(|l| u8::try_from(l).ok())
            .filter(|l| usize::from(*l) < regia_procgen::painted::MAX_PAINT_LAYERS)
            .ok_or_else(|| bad("procgen_tool_bad_gesture", "`brush.layer` is an integer 0-7"))?,
    };
    Ok(Brush {
        radius_m: f("radius_m", d.radius_m),
        spacing_m: f("spacing_m", d.spacing_m),
        erase: b.get("erase").and_then(Value::as_bool).unwrap_or(false),
        layer,
        density: f("density", d.density),
        random_yaw: b
            .get("random_yaw")
            .and_then(Value::as_bool)
            .unwrap_or(d.random_yaw),
        seed: match b.get("seed") {
            None | Some(Value::Null) => d.seed,
            Some(v) => v
                .as_u64()
                .and_then(|s| u32::try_from(s).ok())
                .ok_or_else(|| bad("procgen_tool_bad_gesture", "`brush.seed` is a u32"))?,
        },
    })
}

/// A viewport gesture: `{ kind, origin?, direction?, at?, radius_m?, layer?, erase? }`. A pointer
/// gesture takes a ray (`origin`, `direction`) or `at` (a ray straight down onto that point).
fn gesture_param(g: &Value) -> Result<tools::ToolEvent, RpcError> {
    use tools::{PointerPhase, ToolEvent};
    let kind = g
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("procgen_tool_bad_gesture", "`gesture.kind` is required"))?;
    let ray = || -> Result<(Vec3, Vec3), RpcError> {
        if let Some(at) = g.get("at").and_then(vec3) {
            return Ok((at + Vec3::Y * 10_000.0, Vec3::NEG_Y));
        }
        g.get("origin")
            .and_then(vec3)
            .zip(g.get("direction").and_then(vec3))
            .ok_or_else(|| {
                bad(
                    "procgen_tool_bad_gesture",
                    "a pointer gesture takes `at: [x,y,z]` or `origin` and `direction`",
                )
            })
    };
    let pointer = |phase: PointerPhase| -> Result<ToolEvent, RpcError> {
        let (origin, direction) = ray()?;
        Ok(ToolEvent::Pointer {
            phase,
            origin,
            direction,
        })
    };
    match kind {
        "press" => pointer(PointerPhase::Press),
        "move" => pointer(PointerPhase::Move),
        "release" => pointer(PointerPhase::Release),
        "backspace" => Ok(ToolEvent::Backspace),
        "close" => Ok(ToolEvent::Close),
        "enter" => Ok(ToolEvent::Enter),
        "escape" => Ok(ToolEvent::Escape),
        "brush" => Ok(ToolEvent::Brush {
            radius_m: g.get("radius_m").and_then(Value::as_f64).map(|r| r as f32),
            layer: match g.get("layer") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .and_then(|l| u8::try_from(l).ok())
                        .filter(|l| usize::from(*l) < regia_procgen::painted::MAX_PAINT_LAYERS)
                        .ok_or_else(|| {
                            bad("procgen_tool_bad_gesture", "`gesture.layer` is an integer 0-7")
                        })?,
                ),
            },
            erase: g.get("erase").and_then(Value::as_bool),
        }),
        other => Err(bad(
            "procgen_tool_bad_gesture",
            format!(
                "unknown gesture `{other}` (press | move | release | backspace | close | enter | escape | brush)"
            ),
        )),
    }
}

/// `editor.procgen_tool` with `mode` and/or `gesture`: arms (or leaves, `mode: "off"`) the viewport
/// tool mode "Procedural" and feeds it one gesture, the same path the mouse takes.
fn tool_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match params.get("mode") {
        None | Some(Value::Null) => {}
        Some(Value::String(off)) if off == "off" => tools::disarm(world),
        Some(m) if m.is_object() => {
            let name = str_param(m, "tool", "procgen_tool_bad_gesture")?;
            let kind = ToolKind::parse(name).ok_or_else(|| {
                bad(
                    "procgen_tool_bad_gesture",
                    format!("unknown tool `{name}` (path | area | paint | box)"),
                )
            })?;
            let preset = str_param(m, "preset", "procgen_unknown_graph")?.to_string();
            if super::doc::graph_text(world, &preset).is_none() && super::presets::preset(&preset).is_none() {
                return Err(bad(
                    "procgen_unknown_graph",
                    format!("cannot read preset `{preset}`"),
                ));
            }
            let biome = m.get("biome").and_then(Value::as_str).map(str::to_string);
            let brush = match m.get("brush").filter(|b| b.is_object()) {
                Some(b) => Some(brush_param(b)?),
                None => None,
            };
            tools::arm_with(world, kind, &preset, biome, brush);
        }
        Some(_) => {
            return Err(bad(
                "procgen_tool_bad_gesture",
                "`mode` is { tool, preset, biome?, brush? } or \"off\"",
            ));
        }
    }
    let mut result = json!({ "outcome": null, "cancelled": false });
    if let Some(g) = params.get("gesture").filter(|g| !g.is_null()) {
        let event = gesture_param(g)?;
        let out = tools::handle_event(world, event).map_err(rpc)?;
        result["cancelled"] = json!(out.cancelled);
        if let Some(o) = out.outcome {
            result["outcome"] = json!({
                "entity": o.entity.to_bits(),
                "created": o.created,
                "applied": o.applied,
                "painted": o.painted,
                "added": o.added,
                "removed": o.removed,
                "spline_points": o.spline_points,
            });
        }
    }
    let mode = world.get_resource::<tools::ProcViewportMode>();
    result["armed"] = mode.map_or(Value::Null, |m| {
        json!({
            "tool": m.kind.name(),
            "preset": m.preset,
            "biome": m.biome,
            "brush": {
                "radius_m": m.brush.radius_m,
                "spacing_m": m.brush.spacing_m,
                "layer": m.brush.layer,
                "erase": m.brush.erase,
                "random_yaw": m.brush.random_yaw,
                "seed": m.brush.seed,
            },
            "error": (!m.last_error.is_empty()).then(|| m.last_error.clone()),
        })
    });
    result["session"] = world
        .get_resource::<tools::ProcToolSession>()
        .and_then(|s| s.0.as_ref().map(|s| s.target.to_bits()))
        .map_or(Value::Null, |e| json!(e));
    result["pending_strokes"] = json!(tools::pending_strokes(world));
    Ok(result)
}

fn tool(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    if params.get("tool").is_none() && (params.get("mode").is_some() || params.get("gesture").is_some()) {
        return tool_mode(world, params);
    }
    let name = str_param(params, "tool", "procgen_tool_bad_gesture")?;
    let kind = ToolKind::parse(name).ok_or_else(|| {
        bad(
            "procgen_tool_bad_gesture",
            format!("unknown tool `{name}` (path | area | paint | box)"),
        )
    })?;
    let preset = str_param(params, "preset", "procgen_unknown_graph")?.to_string();
    let target = match params.get("target") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .filter(|e| world.get_entity(*e).is_ok())
                .ok_or_else(|| bad("procgen_tool_target_unfit", "`target` is not a live entity"))?,
        ),
    };
    let points = match params.get("points") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => v
            .as_array()
            .ok_or_else(|| bad("procgen_tool_bad_gesture", "`points` is a list of [x, y, z]"))?
            .iter()
            .map(|p| vec3(p).ok_or_else(|| bad("procgen_tool_bad_gesture", "each point is [x, y, z]")))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let brush = match params.get("brush").filter(|b| b.is_object()) {
        None => None,
        Some(b) => Some(brush_param(b)?),
    };
    let box_extents = match params.get("box") {
        Some(b) if b.is_object() => Some(
            b.get("min")
                .and_then(vec3)
                .zip(b.get("max").and_then(vec3))
                .ok_or_else(|| {
                    bad(
                        "procgen_tool_bad_gesture",
                        "`box` is { min: [x,y,z], max: [x,y,z] }",
                    )
                })?,
        ),
        _ => None,
    };
    let move_point = match params.get("move") {
        None | Some(Value::Null) => None,
        Some(m) => Some(
            m.get("index")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .zip(m.get("to").and_then(vec3))
                .ok_or_else(|| bad("procgen_tool_bad_gesture", "`move` is { index, to: [x,y,z] }"))?,
        ),
    };
    let remove_point = match params.get("remove") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| bad("procgen_tool_bad_gesture", "`remove` is a control point index"))?,
        ),
    };
    let gesture = ToolGesture {
        points,
        closed: params.get("closed").and_then(Value::as_bool),
        brush,
        box_extents,
        move_point,
        remove_point,
        clear: params.get("clear").and_then(Value::as_bool).unwrap_or(false),
        biome: params.get("biome").and_then(Value::as_str).map(str::to_string),
    };
    let apply = params.get("apply").and_then(Value::as_bool).unwrap_or(false);
    let out = tools::run_tool(world, kind, &preset, target, &gesture, apply).map_err(rpc)?;
    let state = world
        .get::<regia_procgen::world::ProcGenState>(out.entity)
        .map(|s| s.state.name());
    Ok(json!({
        "entity": out.entity.to_bits(),
        "created": out.created,
        "applied": out.applied,
        "painted": out.painted,
        "added": out.added,
        "removed": out.removed,
        "spline_points": out.spline_points,
        "state": state,
    }))
}

fn tool_cancel(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let cancelled = tools::finish(world, false).map_err(rpc)?;
    Ok(json!({ "cancelled": cancelled }))
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad("procgen_bad_view", format!("`{name}` must be a boolean"))),
    }
}

fn view_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    super::install_if_missing(world);
    let around = bool_param(params, "generate_around_view")?;
    let overlay = bool_param(params, "tiles_overlay")?;
    let volumes = bool_param(params, "show_volumes")?;
    let biomes = bool_param(params, "biome_overlay")?;
    let tint = bool_param(params, "biome_tint")?;
    let bands = bool_param(params, "biome_bands")?;
    let feet = bool_param(params, "biome_footprints")?;
    let filtered = bool_param(params, "biome_filtered")?;
    super::view::set_switches(world, around, overlay, volumes, biomes);
    super::view::set_biome_views(world, tint, bands, feet, filtered);
    let preview = world.resource::<ProcPreview>();
    let (a, o, v, b, views) = (
        preview.generate_around_view,
        preview.tiles_overlay,
        preview.show_volumes,
        preview.biome_overlay,
        preview.biome_views,
    );
    let anchors = world
        .get_resource::<regia_procgen::tiles::ProcGenExtraAnchors>()
        .map_or(0, |e| e.0.len());
    Ok(json!({
        "generate_around_view": a,
        "tiles_overlay": o,
        "show_volumes": v,
        "biome_overlay": b,
        "biome_tint": views.tint,
        "biome_bands": views.bands,
        "biome_footprints": views.footprints,
        "biome_filtered": views.filtered,
        "anchors": anchors,
    }))
}

fn biome_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use super::biome_overlay::{BiomeViews, MAX_FOOTPRINTS, filtered_at, gather};
    super::install_if_missing(world);
    let at = match params.get("at") {
        None | Some(Value::Null) => None,
        Some(Value::Array(v)) if v.len() == 2 => match (v[0].as_f64(), v[1].as_f64()) {
            (Some(x), Some(z)) if x.is_finite() && z.is_finite() => Some((x as f32, z as f32)),
            _ => return Err(bad("procgen_bad_view", "`at` must be [x, z] numbers")),
        },
        Some(_) => return Err(bad("procgen_bad_view", "`at` must be [x, z] numbers")),
    };
    let radius = match params.get("radius_m") {
        None | Some(Value::Null) => 1.0,
        Some(v) => match v.as_f64() {
            Some(r) if r.is_finite() && r > 0.0 => r as f32,
            _ => return Err(bad("procgen_bad_view", "`radius_m` must be a positive number")),
        },
    };
    let views = world
        .get_resource::<ProcPreview>()
        .map(|p| p.biome_views)
        .unwrap_or_default();
    // Read every view, drawn or not.
    let state = gather(
        world,
        BiomeViews {
            tint: true,
            bands: true,
            footprints: true,
            filtered: true,
        },
    );
    let mut biomes = std::collections::BTreeMap::<String, usize>::new();
    for c in state.cells.iter() {
        *biomes.entry(c.biome.clone()).or_default() += 1;
    }
    let mut filters = std::collections::BTreeMap::<String, usize>::new();
    for m in &state.filtered {
        *filters.entry(format!("{}|{}", m.entry, m.filter)).or_default() += 1;
    }
    let hover = at
        .and_then(|(x, z)| filtered_at(&state.filtered, x, z, radius))
        .map(|m| json!({ "entry": m.entry, "filter": m.filter, "position": m.position }));
    Ok(json!({
        "views": {
            "tint": views.tint,
            "bands": views.bands,
            "footprints": views.footprints,
            "filtered": views.filtered,
        },
        "claim_cells": state.cells.len(),
        "band_cells": state.cells.iter().filter(|c| c.band).count(),
        "draped_cells": state.cells.iter().filter(|c| c.draped).count(),
        "biomes": biomes,
        "footprints": state.footprints_total,
        "footprints_drawn": state.footprints.len().min(MAX_FOOTPRINTS),
        "filtered": state.filtered.len(),
        "filters": filters,
        "hover": hover,
    }))
}

/// `editor.procgen_widgets`: the node settings panel's typed widgets, and a dry-run check.
fn widgets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use super::widgets as w;
    require_open(world)?;
    let node = u32_param(params, "node").ok_or_else(|| bad("procgen_unknown_node", "`node` is required"))?;
    let registry = doc::registry(world);
    let d = world.resource::<ProcGraphDoc>();
    let n = d
        .graph
        .nodes
        .get(&node)
        .ok_or_else(|| bad("procgen_unknown_node", format!("no node {node}")))?;
    let search = params.get("search").and_then(Value::as_str).unwrap_or("");
    let settings: Vec<Value> = registry.get(&n.ty).map_or_else(Vec::new, |kind| {
        kind.settings()
            .iter()
            .map(|desc| {
                let value = n
                    .settings
                    .get(desc.name)
                    .cloned()
                    .or_else(|| regia_procgen::SettingValue::parse(desc.default).ok())
                    .unwrap_or(regia_procgen::SettingValue::Ident("None".into()));
                let widget = w::widget_kind(desc.ty);
                let mut row = json!({
                    "name": desc.name,
                    "widget": widget.name(),
                    "value": value.write(),
                    "bound": value.as_bind(),
                    "min": desc.min,
                    "max": desc.max,
                    "doc": desc.doc,
                });
                match (widget, desc.ty) {
                    (_, regia_procgen::nodes::SettingType::Enum(options)) => row["options"] = json!(options),
                    (w::WidgetKind::Curve, _) => row["points"] = json!(w::curve_keys(&value)),
                    (w::WidgetKind::NameList, _) => row["items"] = json!(w::name_list(&value)),
                    (w::WidgetKind::Asset, _) => row["choices"] = json!(w::asset_choices(world, search)),
                    _ => {}
                }
                row
            })
            .collect()
    });
    let mut out = json!({ "node": node, "ty": n.ty, "settings": settings });
    if let Some(check) = params.get("check") {
        let setting = str_param(check, "setting", "unknown_setting")?;
        let value = json_setting(check.get("value").unwrap_or(&Value::Null))
            .map_err(|m| bad("procgen_param_value", m))?;
        out["check"] = match w::check_setting(world, node, setting, &value) {
            Ok(()) => json!({ "ok": true, "reason": null, "message": null }),
            Err(e) => json!({ "ok": false, "reason": e.code, "message": e.message }),
        };
    }
    Ok(out)
}

/// `editor.procgen_badges`: node badges and theme pin colors.
fn badges(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use super::badges as b;
    require_open(world)?;
    let only = match params.get("nodes") {
        None | Some(Value::Null) => None,
        Some(_) => Some(node_ids(params, "nodes")?),
    };
    let map = b::node_badges(world, only.as_ref());
    let d = world.resource::<ProcGraphDoc>();
    let pins = super::panel::graph_pins(world, &d.graph);
    let tokens = crate::theme::tokens(world);
    let pin_rows = |list: &[regia_procgen::nodes::PinDesc]| -> Vec<Value> {
        list.iter()
            .map(|p| {
                let colors: Vec<String> = b::pin_colors(tokens, p.kinds).into_iter().map(b::hex).collect();
                json!({ "name": p.name, "colors": colors })
            })
            .collect()
    };
    let nodes: Vec<Value> = map
        .iter()
        .map(|(id, nb)| {
            let (inputs, outputs) = pins.get(id).map_or((Vec::new(), Vec::new()), |p| {
                (pin_rows(&p.inputs), pin_rows(&p.outputs))
            });
            json!({
                "id": id,
                "badges": nb.list.iter().map(b::Badge::name).collect::<Vec<_>>(),
                "warnings": nb.warnings,
                "tier": nb.tier.map(|t| json!({
                    "index": t.index,
                    "cell_m": t.cell_m,
                    "shared": t.shared,
                    "feeds": t.feeds_list(),
                    "neutral": t.neutral,
                })),
                "cache_hit": nb.cache_hit,
                "error": nb.error.as_ref().map(|(c, m)| json!({ "reason": c, "message": m })),
                "pins": { "inputs": inputs, "outputs": outputs },
            })
        })
        .collect();
    Ok(json!({ "nodes": nodes }))
}

/// `editor.procgen_promote`: promote to parameter with a name.
fn promote(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = u32_param(params, "node").ok_or_else(|| bad("procgen_unknown_node", "`node` is required"))?;
    let setting = str_param(params, "setting", "unknown_setting")?.to_string();
    let name = match params.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| bad("procgen_param_name", "`name` is a string"))?
                .to_string(),
        ),
    };
    let existing = params.get("existing").and_then(Value::as_bool).unwrap_or(false);
    let (param, created) =
        doc::promote_setting(world, node, &setting, name.as_deref(), existing).map_err(rpc)?;
    Ok(json!({ "param": param, "created": created, "node": node, "setting": setting }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `out` names its permission now that the handler enforces it (spec 67 § 6, ADR-5646); the
    /// method stays `read` because a capture without `out` only reads.
    #[test]
    fn procgen_capture_out_states_the_permission_it_enforces() {
        let schema = methods()
            .into_iter()
            .find(|(method, _)| method.name == "editor.procgen_capture")
            .map(|(method, _)| method)
            .expect("schema");
        assert_eq!(schema.permission, Permission::Read);
        let out = schema
            .params
            .iter()
            .find(|param| param.name == "out")
            .expect("out");
        assert!(out.description.contains("edit_scene"), "{}", out.description);
        assert!(out.description.contains("project"), "{}", out.description);
    }

    /// corr-editor-a L1, fixed at integration (ADR-5712): the size text names the clamp
    /// `capture_dim` applies (64..=4096, the offscreen renderer's limit), not the old 1024.
    #[test]
    fn procgen_capture_size_text_names_the_clamp_the_handler_applies() {
        let schema = methods()
            .into_iter()
            .find(|(method, _)| method.name == "editor.procgen_capture")
            .map(|(method, _)| method)
            .expect("schema");
        for (name, default) in [("width", 1000u32), ("height", 640u32)] {
            let param = schema
                .params
                .iter()
                .find(|param| param.name == name)
                .expect("size param");
            assert!(param.description.contains("64-4096"), "{}", param.description);
            assert!(
                param.description.contains(&format!("default {default}")),
                "{}",
                param.description
            );
            assert_eq!(crate::agent_ext::capture_dim(&json!({}), name, default), default);
            assert_eq!(
                crate::agent_ext::capture_dim(&json!({ name: 99_999 }), name, default),
                4096
            );
            assert_eq!(
                crate::agent_ext::capture_dim(&json!({ name: 1 }), name, default),
                64
            );
        }
    }

    #[test]
    fn procgen_capture_documents_the_range_capture_dim_allows() {
        // corr-editor-a L1: the schema said 64-1024 while `agent_ext::capture_dim` clamps to
        // 64..=4096, so an agent following the docs was surprised by a 2048-wide capture.
        let schema = methods()
            .into_iter()
            .find(|(method, _)| method.name == "editor.procgen_capture")
            .map(|(method, _)| method)
            .expect("schema");
        for name in ["width", "height"] {
            let param = schema.params.iter().find(|p| p.name == name).expect(name);
            assert!(
                param.description.contains("64-4096"),
                "{name} lies about the clamp: {}",
                param.description
            );
        }
    }
}
