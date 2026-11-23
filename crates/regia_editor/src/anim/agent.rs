// SPDX-License-Identifier: MPL-2.0
//! The animation editor's agent-protocol surface: `editor.anim_*` methods that build and
//! inspect graphs, author clips and skeletons, drive the preview, read PIE debugging and
//! capture the panels (`book/agents/animation-editor.md`).
//!
//! Every mutating method goes through [`super::edit`], so it lands on the editor's undo stack
//! exactly like a panel edit (`editor.undo` reverts it). Node data travels as the serde JSON
//! of the node kind's fields (`{"clip": "Run", "looping": true}`), merged shallowly into the
//! node's current data.

use serde_json::{Map, Value, json};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_animation::clip::{Notify, RootMotionMode};
use regia_animation::components::spawn_skeleton;
use regia_animation::curve::{Curve, CurveKey, CurveKeyKind};
use regia_animation::gait_asset::GaitAssetError;
use regia_animation::gait_rig::GaitRig;
use regia_animation::params::ParamValue;
use regia_animation::sample::{SampleMode, sample_into};
use regia_animation::skeleton::{LocalBind, SkeletonSocket};
use regia_animation::state_machine::TransitionDef;
use regia_ecs::prelude::*;

use super::model::{NodeId, NodeKind, Scope};
use super::timeline::{MarkerKind, Selection, Timeline, TimelineEvent, TimelineMarker, TimelineTrack};
use super::{AnimEditor, PreviewRig, edit};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

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
    mutating: bool,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating,
        permission: if mutating {
            Permission::EditScene
        } else {
            Permission::Read
        },
    }
}

const SCOPE: ParamSchema = p(
    "scope",
    "object",
    false,
    "Canvas: omit (or \"root\") for the root, {machine: id, state: name} for a state's sub-graph.",
);

/// Every animation-editor method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.anim_open",
                "anim_open",
                "Open a .animgraph.ron in the anim graph editor, or start a new graph (create: true).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            false,
                            "Project-relative (assets/…) or absolute .animgraph.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start an empty graph (saved to `path` later).",
                        ),
                    ]
                },
                "{ path, nodes, parameters }",
                true,
            ),
            anim_open as Handler,
        ),
        (
            m(
                "editor.anim_graph",
                "anim_graph",
                "The open graph: nodes (id, kind, scope, pos, inputs, data), parameters, outputs, compile/validation status and the RON it would save.",
                &const {
                    [p(
                        "ron",
                        "boolean",
                        false,
                        "Include the compiled .animgraph.ron text.",
                    )]
                },
                "{ path, dirty, parameters, output, nodes, valid, error, unreachable, paths, ron? }",
                false,
            ),
            anim_graph as Handler,
        ),
        (
            m(
                "editor.anim_param",
                "anim_param",
                "Declare (or re-default) a graph parameter, or remove one; or, with live_value, write a declared parameter of the animator that play-in-editor runs (a debug write: not saved, cleared when play stops).",
                &const {
                    [
                        p("name", "string", true, "Parameter name."),
                        p(
                            "live_value",
                            "any",
                            false,
                            "Write this value (number, bool, [x, y(, z)], or anything for a trigger) to the watched play animator instead of declaring. Errors: not_playing, unknown_parameter.",
                        ),
                        p(
                            "kind",
                            "string",
                            false,
                            "float | int | bool | vec2 | vec3 | trigger (default float).",
                        ),
                        p("default", "any", false, "Default: number, bool or [x, y(, z)]."),
                        p("remove", "boolean", false, "Remove the declaration instead."),
                    ]
                },
                "{ parameters, live? }",
                true,
            ),
            anim_param as Handler,
        ),
        (
            m(
                "editor.anim_record",
                "anim_record",
                "The animation recorder: op start (a ring of `capacity` frames of the watched play animator's state, transition, parameters, node weights and pose), stop, or state.",
                &const {
                    [
                        p("op", "string", true, "start | stop | state."),
                        p(
                            "capacity",
                            "number",
                            false,
                            "start: frames kept (default 600, at most 36000).",
                        ),
                    ]
                },
                "{ recording, frames, capacity, bytes, dropped, scrub, first, last }",
                true,
            ),
            anim_record as Handler,
        ),
        (
            m(
                "editor.anim_scrub",
                "anim_scrub",
                "Replay a recorded frame: the graph panel highlights its states and nodes and the preview takes its pose; frame null follows play again. Errors: nothing_recorded, frame_out_of_range.",
                &const {
                    [p(
                        "frame",
                        "number",
                        true,
                        "Position in the recording (0 = oldest kept), or null for live.",
                    )]
                },
                "{ frame, index, play_frame, entity, state, transition, parameters, weights, pose_hash } | { frame: null }",
                true,
            ),
            anim_scrub as Handler,
        ),
        (
            m(
                "editor.curve_view",
                "curve_view",
                "The multi-curve views: list the curves of the clip panel (the open clip's curves and property tracks) or of the sequence curve panel (every float and vec3 component), and show, hide, lock or unlock one. View state, not undoable. Error: unknown_curve.",
                &const {
                    [
                        p("panel", "string", true, "anim_clip | seq_curve."),
                        p(
                            "curve",
                            "string",
                            false,
                            "The curve's key from the list (curve:<name>, property:<path>, track:<index>:<component>).",
                        ),
                        p("visible", "boolean", false, "Show (true) or hide (false) it."),
                        p("locked", "boolean", false, "Lock (true) or unlock (false) it."),
                    ]
                },
                "{ panel, curves: [{ key, label, palette_index, visible, locked, edited }] }",
                true,
            ),
            curve_view as Handler,
        ),
        (
            m(
                "editor.anim_add_node",
                "anim_add_node",
                "Add a node: clip, blend_space_1d, blend_space_2d, state_machine, blend, additive, layer, two_bone_ik, look_at, root_motion, slot, chain_ik, body_aim, stride_warp, orientation_warp.",
                &const {
                    [
                        p("kind", "string", true, "Node kind."),
                        SCOPE,
                        p("pos", "array", false, "[x, y] canvas position."),
                        p(
                            "data",
                            "object",
                            false,
                            "Kind fields, e.g. {clip: \"Run\", looping: true, speed: 1, sync_group: \"gait\"}.",
                        ),
                    ]
                },
                "{ node }",
                true,
            ),
            anim_add_node as Handler,
        ),
        (
            m(
                "editor.anim_set_node",
                "anim_set_node",
                "Change a node's kind fields (shallow merge) and/or position.",
                &const {
                    [
                        p("node", "number", true, "Node id."),
                        p("data", "object", false, "Fields to replace."),
                        p("pos", "array", false, "[x, y]."),
                    ]
                },
                "{ node, data }",
                true,
            ),
            anim_set_node as Handler,
        ),
        (
            m(
                "editor.anim_remove_node",
                "anim_remove_node",
                "Remove a node (a state machine takes its states' sub-graphs with it).",
                &const { [p("node", "number", true, "Node id.")] },
                "{ removed }",
                true,
            ),
            anim_remove_node as Handler,
        ),
        (
            m(
                "editor.anim_connect",
                "anim_connect",
                "Connect `child` into `parent`'s input `slot` (a, b, base, additive, layer<i>, inner, or an index), or — without `parent` — make `child` its canvas's output.",
                &const {
                    [
                        p("child", "number", true, "The node whose pose flows."),
                        p("parent", "number", false, "The consuming node."),
                        p("slot", "string", false, "Input name or index (default 0)."),
                    ]
                },
                "{ connected }",
                true,
            ),
            anim_connect as Handler,
        ),
        (
            m(
                "editor.anim_disconnect",
                "anim_disconnect",
                "Clear `parent`'s input `slot`, or a canvas's output (`scope`, without parent).",
                &const {
                    [
                        p("parent", "number", false, "The consuming node."),
                        p("slot", "string", false, "Input."),
                        SCOPE,
                    ]
                },
                "{ disconnected }",
                true,
            ),
            anim_disconnect as Handler,
        ),
        (
            m(
                "editor.anim_state",
                "anim_state",
                "Edit a state machine's states: op add | remove | rename | initial | move.",
                &const {
                    [
                        p("machine", "number", true, "State machine node id."),
                        p("op", "string", true, "add | remove | rename | initial | move."),
                        p("name", "string", true, "State name."),
                        p("new_name", "string", false, "For rename."),
                        p("pos", "array", false, "[x, y] on the state canvas (add / move)."),
                    ]
                },
                "{ states, initial }",
                true,
            ),
            anim_state as Handler,
        ),
        (
            m(
                "editor.anim_transition",
                "anim_transition",
                "Add, replace or remove a transition. Conditions are serde JSON: {Float: {param, op: Gt, value}}, {Bool: {param, value}}, {Trigger: {param}}, \"ClipFinished\", {Notify: name}.",
                &const {
                    [
                        p("machine", "number", true, "State machine node id."),
                        p("op", "string", false, "add (default) | set | remove."),
                        p("index", "number", false, "Transition index (set / remove)."),
                        p("from", "string", false, "Source state; omit or null for Any."),
                        p("to", "string", false, "Target state (add / set)."),
                        p("conditions", "array", false, "All must hold."),
                        p("duration", "number", false, "Cross-fade seconds (default 0.2)."),
                        p("curve", "string", false, "Linear | Smooth (default Smooth)."),
                        p("interruptible", "boolean", false, "Default true."),
                        p(
                            "blend",
                            "string",
                            false,
                            "Crossfade (default) | Inertialize | InertializeSpring. `duration` is the inertial decay time.",
                        ),
                    ]
                },
                "{ index, transitions }",
                true,
            ),
            anim_transition as Handler,
        ),
        (
            m(
                "editor.anim_save",
                "anim_save",
                "Validate and save the graph (.animgraph.ron + editor layout sidecar).",
                &const { [p("path", "string", false, "Save-as path; default the open path.")] },
                "{ path, bytes, unreachable }",
                true,
            ),
            anim_save as Handler,
        ),
        (
            m(
                "editor.anim_rig",
                "anim_rig",
                "Choose the preview rig: demo (built-in biped), selected (the selected entity's SkinnedMesh), gltf (a .glb). Opens its .animedit.ron sidecar.",
                &const {
                    [
                        p(
                            "source",
                            "string",
                            false,
                            "demo | selected | gltf (default demo).",
                        ),
                        p("path", "string", false, "The .glb for gltf."),
                        p("sidecar", "string", false, "Override the .animedit.ron path."),
                    ]
                },
                "{ name, joints, clips, sockets, ik_chains, sidecar }",
                true,
            ),
            anim_rig as Handler,
        ),
        (
            m(
                "editor.anim_preview",
                "anim_preview",
                "Set preview parameters and advance the preview; returns the current state, active nodes and weights.",
                &const {
                    [
                        p(
                            "params",
                            "object",
                            false,
                            "{name: value}; triggers fire with true or \"fire\".",
                        ),
                        p("advance", "number", false, "Seconds to advance (60 Hz steps)."),
                        p(
                            "speed",
                            "number",
                            false,
                            "Preview speed, 0..=2 (the panel's speed slider; scales `advance`).",
                        ),
                    ]
                },
                "{ state, error, nodes: [{path, node, active, weight, state}], parameters, speed }",
                true,
            ),
            anim_preview as Handler,
        ),
        (
            m(
                "editor.anim_debug",
                "anim_debug",
                "PIE debugging: the running animator's snapshot for the open graph (current state, transition progress, node weights, parameter values). Null when PIE is not running it.",
                &[],
                "{ entity, name, state, nodes, parameters } | null",
                false,
            ),
            anim_debug as Handler,
        ),
        (
            m(
                "editor.anim_clip",
                "anim_clip",
                "Author a clip of the preview rig: op add_notify | set_notify | remove_notify | root_motion | set_curve | remove_curve; set the clip preview: op preview (speed, loop, range, onion, root_floor, time; view state, not undoable); list the rig's clips: op list (filter, sort).",
                &const {
                    [
                        p("clip", "string", false, "Clip name (every op but list)."),
                        p("op", "string", true, "Operation."),
                        p("index", "number", false, "Notify index (set / remove)."),
                        p(
                            "time",
                            "number",
                            false,
                            "Notify time, or the preview playhead (preview), seconds.",
                        ),
                        p(
                            "name",
                            "string",
                            false,
                            "Notify name, curve name or property path.",
                        ),
                        p("payload", "string", false, "Notify payload (RON)."),
                        p("mode", "string", false, "None | XZ | XYZ | Yaw (root_motion)."),
                        p(
                            "property",
                            "boolean",
                            false,
                            "set_curve / remove_curve: a property track instead of a named curve.",
                        ),
                        p(
                            "keys",
                            "array",
                            false,
                            "set_curve: [[time, value, \"linear\"|\"constant\"|\"cubic\", slope?], …].",
                        ),
                        p("speed", "number", false, "preview: playback speed, 0.1..=4."),
                        p("loop", "string", false, "preview: once | loop | ping_pong."),
                        p(
                            "range",
                            "array",
                            false,
                            "preview: the trimmed preview range [start, end] seconds, or null for the whole clip (error bad_range).",
                        ),
                        p(
                            "onion",
                            "object",
                            false,
                            "preview: { enabled, before, after, step, opacity } ghost poses.",
                        ),
                        p(
                            "root_floor",
                            "boolean",
                            false,
                            "preview: scroll the floor with root motion and draw the trajectory.",
                        ),
                        p(
                            "advance",
                            "number",
                            false,
                            "preview: play this many seconds (60 Hz steps) from `time`.",
                        ),
                        p(
                            "filter",
                            "string",
                            false,
                            "list: substring filter (case-insensitive).",
                        ),
                        p(
                            "sort",
                            "string",
                            false,
                            "list: imported | name | length | recent.",
                        ),
                    ]
                },
                "{ clip, notifies, curves, property_tracks, root_motion } | preview: { clip, time, playing, speed, loop, range, onion, onion_times, root_floor, root } | list: { clips: [{ name, duration }] }",
                true,
            ),
            anim_clip as Handler,
        ),
        (
            m(
                "editor.anim_skeleton",
                "anim_skeleton",
                "Author the rig's skeleton: op add_socket | remove_socket | add_ik_chain | remove_ik_chain.",
                &const {
                    [
                        p("op", "string", true, "Operation."),
                        p("name", "string", true, "Socket or chain name."),
                        p("joint", "string", false, "Socket joint."),
                        p("offset", "array", false, "Socket offset [x, y, z]."),
                        p(
                            "rotation",
                            "array",
                            false,
                            "Socket rotation [yaw, pitch, roll] in degrees (Euler YXZ, the panel's Rotation°).",
                        ),
                        p("joints", "array", false, "IK chain [root, mid, tip]."),
                        p("pole_param", "string", false, "IK chain pole parameter."),
                    ]
                },
                "{ sockets, ik_chains }",
                true,
            ),
            anim_skeleton as Handler,
        ),
        (
            m(
                "editor.anim_save_edits",
                "anim_save_edits",
                "Save the rig's .animedit.ron (notifies, curves, sockets, IK chains).",
                &const {
                    [p(
                        "path",
                        "string",
                        false,
                        "Save-as path; default the rig's sidecar path.",
                    )]
                },
                "{ path }",
                true,
            ),
            anim_save_edits as Handler,
        ),
        (
            m(
                "editor.timeline_set",
                "timeline_set",
                "Set up the standalone timeline widget's state (headless): from a preview-rig clip or by hand.",
                &const {
                    [
                        p(
                            "clip",
                            "string",
                            false,
                            "Load tracks (one per animated joint) and markers (notifies) from this preview-rig clip.",
                        ),
                        p("duration", "number", false, "Seconds."),
                        p("playhead", "number", false, "Seconds."),
                        p("range", "array", false, "[start, end] seconds, or null to clear."),
                        p("loop", "boolean", false, "Looping."),
                        p(
                            "snap",
                            "number",
                            false,
                            "Snap step in seconds, or null / 0 for free.",
                        ),
                        p("tracks", "array", false, "[{ name, keys: [seconds, …] }, …]."),
                        p(
                            "markers",
                            "array",
                            false,
                            "[{ time, label, kind: foot_contact | notify | custom }, …].",
                        ),
                        p(
                            "width",
                            "number",
                            false,
                            "Widget width in points for pointer input (default 600).",
                        ),
                        p(
                            "zoom",
                            "number",
                            false,
                            "Zoom factor around the playhead (>1 zooms in).",
                        ),
                    ]
                },
                "the timeline state, as editor.timeline_state",
                true,
            ),
            timeline_set as Handler,
        ),
        (
            m(
                "editor.timeline_state",
                "timeline_state",
                "Read the standalone timeline: duration, playhead, range, loop, tracks, markers, selection, the view, and the edit events since the last call (cleared).",
                &[],
                "{ duration, playhead, range, loop, snap, selection, view, tracks, markers, events }",
                false,
            ),
            timeline_state as Handler,
        ),
        (
            m(
                "editor.timeline_input",
                "timeline_input",
                "Synthesize pointer input on the timeline by time and row: op press | drag | release | delete. Returns the edit events it caused.",
                &const {
                    [
                        p("op", "string", true, "press | drag | release | delete."),
                        p("time", "number", false, "Seconds under the pointer."),
                        p(
                            "row",
                            "string",
                            false,
                            "toolbar | ruler | markers | a track index (default ruler).",
                        ),
                        p("button", "string", false, "primary (default) | secondary."),
                        p(
                            "shift",
                            "boolean",
                            false,
                            "Shift held (press on the ruler draws the range).",
                        ),
                        p(
                            "double",
                            "boolean",
                            false,
                            "release: completes a double click (adds a key / marker).",
                        ),
                    ]
                },
                "{ events, playhead, range, selection }",
                true,
            ),
            timeline_input as Handler,
        ),
        (
            m(
                "editor.sample_into",
                "sample_into",
                "Sample a preview-rig clip into a scratch copy of the rig with regia_animation::sample_into and read the joint transforms (no animation schedule). Additive layers the clip's delta on the bind pose.",
                &const {
                    [
                        p("clip", "string", true, "Clip name."),
                        p("time", "number", true, "Seconds from the clip start."),
                        p("mode", "string", false, "replace (default) | additive."),
                        p(
                            "joints",
                            "array",
                            false,
                            "Joint names to report (default: every joint the clip animates).",
                        ),
                    ]
                },
                "{ clip, time, mode, joint_count, written, untouched, joints: [{ name, translation, rotation, scale }] }",
                false,
            ),
            sample_into_method as Handler,
        ),
        (
            m(
                "editor.capture_panel",
                "capture_panel",
                "Render an editor panel offscreen: anim_graph, anim_clip, anim_skeleton, undo_history, safe_mode, match_rules or world_settings.",
                &const {
                    [
                        p("panel", "string", true, "Panel name."),
                        p(
                            "width",
                            "number",
                            false,
                            "Pixels, clamped to 64..=4096 (default 1024).",
                        ),
                        p("height", "number", false, "Pixels (default 640)."),
                    ]
                },
                "{ width, height, mime, png_base64 }",
                false,
            ),
            capture_panel as Handler,
        ),
        (
            m(
                "editor.anim_gait_load",
                "anim_gait_load",
                "Load a .gait.ron into an entity's GaitRig, as the Gait authoring section's Load button does: the file's params, legs and body replace the rig in one undoable component edit.",
                &const {
                    [
                        p("entity", "number", true, "Entity id whose GaitRig is replaced."),
                        p("path", "string", true, "Project-relative .gait.ron path to load."),
                    ]
                },
                "{ entity, path, loaded, legs, body, auto_hips }",
                true,
            ),
            anim_gait_load as Handler,
        ),
        (
            m(
                "editor.anim_gait_save",
                "anim_gait_save",
                "Write an entity's GaitRig to a .gait.ron file, as the Gait authoring section's Save .gait.ron button does (atomic write through the format's own writer).",
                &const {
                    [
                        p("entity", "number", true, "Entity id whose GaitRig is written."),
                        p(
                            "path",
                            "string",
                            true,
                            "Project-relative .gait.ron path to write.",
                        ),
                    ]
                },
                "{ entity, path, written, legs, body, auto_hips }",
                true,
            ),
            anim_gait_save as Handler,
        ),
        (
            m(
                "editor.anim_locomotion_preview",
                "anim_locomotion_preview",
                "Walk a gait in the animation preview's locomotion test bed (ADR-8806): an entity's GaitRig, a .gait.ron or the built-in tripod over a ground preset at a set speed and turn rate; answers the quality metrics and, with `screenshot`, writes the test bed as a PNG.",
                &const {
                    [
                        p(
                            "entity",
                            "number",
                            false,
                            "Entity id whose GaitRig settings are walked (not with `asset`).",
                        ),
                        p(
                            "asset",
                            "string",
                            false,
                            "Project-relative .gait.ron to walk (not with `entity`). Neither: the built-in six-leg tripod.",
                        ),
                        p(
                            "ground",
                            "object",
                            false,
                            "A preset name or {kind: flat | slope {degrees} | stairs {rise, run} | rubble {amplitude, cell, seed} | tunnel {depth}} (default flat).",
                        ),
                        p(
                            "speed",
                            "number",
                            false,
                            "Driver ground speed, 0..=20 m/s (default 1).",
                        ),
                        p(
                            "turn_rate",
                            "number",
                            false,
                            "Driver turn rate, -6.283..=6.283 rad/s, + turns left (default 0).",
                        ),
                        p(
                            "seconds",
                            "number",
                            false,
                            "Simulated time at 1/60 s steps, above 0 and at most 300 (default 5).",
                        ),
                        p(
                            "screenshot",
                            "string",
                            false,
                            "Project-relative .png path: render the test bed (gizmos on) offscreen and write it there.",
                        ),
                    ]
                },
                "{ source, ground, speed, turn_rate, seconds, steps, dt, legs, metrics: { max_slide, max_foot_jump, forced_steps, refused, replants, min_planted, stability_margin, min_stability_margin, settled }, final: { position, yaw }, screenshot }",
                false,
            ),
            anim_locomotion_preview as Handler,
        ),
    ]
}

fn bad(message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message)
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("`{name}` (string) is required")))
}

fn id_param(params: &Value, name: &str) -> Result<NodeId, RpcError> {
    params
        .get(name)
        .and_then(Value::as_u64)
        .map(|v| v as NodeId)
        .ok_or_else(|| bad(format!("`{name}` (node id) is required")))
}

fn pos_param(params: &Value) -> Option<[f32; 2]> {
    let a = params.get("pos")?.as_array()?;
    Some([a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32])
}

fn scope_param(params: &Value) -> Result<Scope, RpcError> {
    match params.get("scope") {
        None | Some(Value::Null) => Ok(Scope::Root),
        Some(Value::String(s)) if s == "root" => Ok(Scope::Root),
        Some(v) => {
            let machine =
                v.get("machine")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| bad("`scope.machine` (node id) is required"))? as NodeId;
            let state = v
                .get("state")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("`scope.state` is required"))?;
            Ok(Scope::State {
                machine,
                state: state.to_string(),
            })
        }
    }
}

fn editor(world: &mut World) -> Mut<'_, AnimEditor> {
    if !world.contains_resource::<AnimEditor>() {
        world.init_resource::<AnimEditor>();
    }
    world.resource_mut::<AnimEditor>()
}

/// The kind's fields as a JSON object (the inner value of serde's external tag).
fn kind_data(kind: &NodeKind) -> Value {
    match serde_json::to_value(kind) {
        Ok(Value::Object(map)) => map.into_iter().next().map(|(_, v)| v).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// `kind` with `data`'s fields merged over its current fields.
fn merge_kind(kind: &NodeKind, data: &Value) -> Result<NodeKind, String> {
    let Value::Object(patch) = data else {
        return Err("`data` must be an object".into());
    };
    let tagged = serde_json::to_value(kind).map_err(|e| e.to_string())?;
    let Value::Object(outer) = tagged else {
        return Err("node kind did not serialize to an object".into());
    };
    let Some((tag, inner)) = outer.into_iter().next() else {
        return Err("empty node kind".into());
    };
    let mut fields = match inner {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    for (k, v) in patch {
        fields.insert(k.clone(), v.clone());
    }
    let mut out = Map::new();
    out.insert(tag, Value::Object(fields));
    serde_json::from_value(Value::Object(out)).map_err(|e| format!("bad node data: {e}"))
}

fn scope_json(scope: &Scope) -> Value {
    match scope {
        Scope::Root => json!("root"),
        Scope::State { machine, state } => json!({ "machine": machine, "state": state }),
    }
}

fn graph_json(editor: &AnimEditor, with_ron: bool) -> Value {
    let model = &editor.docs.graph;
    let nodes: Vec<Value> = model
        .nodes
        .iter()
        .map(|(id, n)| {
            let names = n.kind.input_names();
            let inputs: Map<String, Value> = names
                .iter()
                .zip(&n.inputs)
                .map(|(name, child)| (name.clone(), child.map_or(Value::Null, |c| json!(c))))
                .collect();
            let mut node = json!({
                "id": id,
                "kind": n.kind.label(),
                "scope": scope_json(&n.scope),
                "pos": n.pos,
                "inputs": inputs,
            });
            if let NodeKind::StateMachine(machine) = &n.kind {
                node["states"] = json!(
                    machine
                        .states
                        .iter()
                        .map(|s| json!({ "name": s.name, "pos": s.pos, "output": s.output }))
                        .collect::<Vec<_>>()
                );
                node["initial"] = json!(machine.initial);
                node["transitions"] = transitions_json(&machine.transitions);
            } else {
                node["data"] = kind_data(&n.kind);
            }
            node
        })
        .collect();
    let compiled = model.compile_validated();
    let mut out = json!({
        "path": editor.graph_path.as_ref().map(|p| p.display().to_string()),
        "dirty": editor.graph_dirty(),
        "parameters": serde_json::to_value(&model.parameters).unwrap_or(Value::Null),
        "output": model.output,
        "nodes": nodes,
        "valid": compiled.is_ok(),
        "error": compiled.as_ref().err(),
    });
    if let Ok(c) = &compiled {
        out["unreachable"] = json!(c.unreachable);
        out["paths"] = json!(
            c.paths
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect::<Map<_, _>>()
        );
        if with_ron {
            out["ron"] = json!(c.graph.to_ron_string().ok());
        }
    }
    out
}

fn anim_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params.get("path").and_then(Value::as_str).map(str::to_string);
    let full = path
        .as_deref()
        .map(|p| super::graph_panel::agent_path(world, "path", p))
        .transpose()?;
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let mut e = editor(world);
    if create {
        e.new_graph(full);
    } else {
        let full = full.ok_or_else(|| bad("`path` is required unless create: true"))?;
        e.open_graph(&full).map_err(bad)?;
    }
    Ok(json!({
        "path": e.graph_path.as_ref().map(|p| p.display().to_string()),
        "nodes": e.docs.graph.nodes.len(),
        "parameters": e.docs.graph.parameters.len(),
    }))
}

fn anim_graph(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let with_ron = params.get("ron").and_then(Value::as_bool).unwrap_or(false);
    Ok(graph_json(&editor(world), with_ron))
}

/// A parameter value of `kind` from a JSON default.
fn param_value(kind: &str, default: Option<&Value>) -> Result<ParamValue, RpcError> {
    let base = super::graph_panel::param_default(kind)
        .ok_or_else(|| bad(format!("unknown parameter kind {kind:?}")))?;
    let Some(default) = default else {
        return Ok(base);
    };
    let numbers: Vec<f32> = match default {
        Value::Array(items) => items.iter().filter_map(Value::as_f64).map(|v| v as f32).collect(),
        Value::Number(n) => vec![n.as_f64().unwrap_or(0.0) as f32],
        _ => Vec::new(),
    };
    Ok(super::graph_inspector::value_of_kind(
        &base,
        &numbers,
        default.as_bool(),
    ))
}

/// An error with one of this page's lowercase codes (`book/agents/animation-editor.md`).
fn coded(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(regia_agent::protocol::INVALID_PARAMS, code, message, hint)
}

/// A JSON value as a parameter of the declared kind.
fn json_param(kind: ParamValue, value: &Value) -> ParamValue {
    match kind {
        ParamValue::Trigger => ParamValue::Trigger,
        other => {
            let numbers: Vec<f32> = match value {
                Value::Array(items) => items.iter().filter_map(Value::as_f64).map(|v| v as f32).collect(),
                Value::Number(n) => vec![n.as_f64().unwrap_or(0.0) as f32],
                _ => Vec::new(),
            };
            super::graph_inspector::value_of_kind(&other, &numbers, value.as_bool())
        }
    }
}

fn anim_param(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "name")?.to_string();
    if let Some(live) = params.get("live_value") {
        let mut e = editor(world);
        let e = &mut *e;
        if e.pie_debug.is_none() || e.recorder.scrub().is_some() {
            return Err(coded(
                "not_playing",
                "no running animator is watched (play is stopped, or a recording is being replayed)",
                "start play with the graph panel open, or scrub back to live with editor.anim_scrub { frame: null }",
            ));
        }
        let kind = e
            .docs
            .graph
            .parameters
            .iter()
            .find(|d| d.0 == name)
            .map(|d| d.1)
            .ok_or_else(|| {
                coded(
                    "unknown_parameter",
                    format!("parameter {name:?} is not declared"),
                    "declare it with editor.anim_param { name, kind }",
                )
            })?;
        let value = json_param(kind, live);
        e.live.queue(&name, value);
        return Ok(json!({
            "parameters": serde_json::to_value(&e.docs.graph.parameters).unwrap_or(Value::Null),
            "live": { "name": name, "value": serde_json::to_value(value).unwrap_or(Value::Null), "queued": e.live.queued.len() },
        }));
    }
    if params.get("remove").and_then(Value::as_bool).unwrap_or(false) {
        edit(world, &format!("remove parameter {name}"), None, |d| {
            if d.graph.remove_param(&name) {
                Ok(())
            } else {
                Err(format!("no parameter {name}"))
            }
        })
        .map_err(bad)?;
    } else {
        let kind = params.get("kind").and_then(Value::as_str).unwrap_or("float");
        let value = param_value(kind, params.get("default"))?;
        edit(world, &format!("declare parameter {name}"), None, |d| {
            d.graph.set_param(&name, value);
            Ok(())
        })
        .map_err(bad)?;
    }
    let e = editor(world);
    Ok(json!({ "parameters": serde_json::to_value(&e.docs.graph.parameters).unwrap_or(Value::Null) }))
}

fn anim_add_node(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = str_param(params, "kind")?;
    let mut kind = NodeKind::default_of(label).ok_or_else(|| {
        bad(format!(
            "unknown node kind {label:?}; kinds: {}",
            NodeKind::LABELS.join(", ")
        ))
    })?;
    if let Some(data) = params.get("data").filter(|d| !d.is_null()) {
        if matches!(kind, NodeKind::StateMachine(_)) {
            return Err(bad(
                "state machines take no data; use editor.anim_state / anim_transition",
            ));
        }
        kind = merge_kind(&kind, data).map_err(bad)?;
    }
    let scope = scope_param(params)?;
    let pos = pos_param(params);
    let node = edit(world, &format!("add {label}"), None, |d| {
        // Without a position the node goes left of the canvas's leftmost node (graphs flow
        // right to the output), so agent-built canvases do not stack every node at the origin.
        let pos = pos.unwrap_or_else(|| {
            d.graph
                .nodes_in(&scope)
                .map(|(_, n)| n.pos[0])
                .reduce(f32::min)
                .map_or([0.0, 0.0], |x| [x - AUTO_NODE_SPACING, 0.0])
        });
        d.graph.add_node(kind, scope, pos)
    })
    .map_err(bad)?;
    Ok(json!({ "node": node }))
}

/// Horizontal gap between automatically placed nodes (wider than the widest node header).
const AUTO_NODE_SPACING: f32 = 460.0;

fn anim_set_node(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params, "node")?;
    let data = params.get("data").filter(|d| !d.is_null()).cloned();
    let pos = pos_param(params);
    let kind = edit(world, &format!("edit node {id}"), None, |d| {
        let current = d.graph.node(id)?.kind.clone();
        if let Some(data) = &data {
            let next = match &current {
                NodeKind::StateMachine(_) => {
                    let initial = data
                        .get("initial")
                        .and_then(Value::as_str)
                        .ok_or("state machines accept only {initial}; use editor.anim_state")?
                        .to_string();
                    let machine = d.graph.machine_mut(id)?;
                    if !machine.states.iter().any(|s| s.name == initial) {
                        return Err(format!("no state {initial:?}"));
                    }
                    machine.initial = initial;
                    None
                }
                other => Some(merge_kind(other, data)?),
            };
            if let Some(next) = next {
                d.graph.set_kind(id, next)?;
            }
        }
        if let Some(pos) = pos {
            d.graph.move_node(id, pos)?;
        }
        Ok(d.graph.node(id)?.kind.clone())
    })
    .map_err(bad)?;
    Ok(json!({ "node": id, "data": kind_data(&kind) }))
}

fn anim_remove_node(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params, "node")?;
    edit(world, &format!("remove node {id}"), None, |d| {
        d.graph.remove_node(id)
    })
    .map_err(bad)?;
    Ok(json!({ "removed": id }))
}

fn slot_param(params: &Value) -> String {
    match params.get("slot") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "0".into(),
    }
}

fn anim_connect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let child = id_param(params, "child")?;
    let parent = params.get("parent").and_then(Value::as_u64).map(|v| v as NodeId);
    let slot = slot_param(params);
    edit(world, "connect", None, |d| match parent {
        Some(parent) => d.graph.connect(child, parent, &slot),
        None => {
            let scope = d.graph.node(child)?.scope.clone();
            d.graph.set_output(&scope, Some(child))
        }
    })
    .map_err(bad)?;
    Ok(json!({ "connected": true }))
}

fn anim_disconnect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let parent = params.get("parent").and_then(Value::as_u64).map(|v| v as NodeId);
    let slot = slot_param(params);
    let scope = scope_param(params)?;
    edit(world, "disconnect", None, |d| match parent {
        Some(parent) => d.graph.disconnect(parent, &slot),
        None => d.graph.set_output(&scope, None),
    })
    .map_err(bad)?;
    Ok(json!({ "disconnected": true }))
}

fn anim_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let machine = id_param(params, "machine")?;
    let op = str_param(params, "op")?.to_string();
    let name = str_param(params, "name")?.to_string();
    let new_name = params.get("new_name").and_then(Value::as_str).map(str::to_string);
    let pos = pos_param(params);
    edit(world, &format!("state {op} {name}"), None, |d| {
        match op.as_str() {
            "add" => {
                let count = d.graph.machine(machine)?.states.len() as f32;
                d.graph
                    .add_state(machine, &name, pos.unwrap_or([count * 170.0 - 170.0, 0.0]))
            }
            "remove" => d.graph.remove_state(machine, &name),
            "rename" => d.graph.rename_state(
                machine,
                &name,
                new_name.as_deref().ok_or("`new_name` is required")?,
            ),
            "initial" => {
                let m = d.graph.machine_mut(machine)?;
                if !m.states.iter().any(|s| s.name == name) {
                    return Err(format!("no state {name:?}"));
                }
                m.initial = name.clone();
                Ok(())
            }
            "move" => {
                let pos = pos.ok_or("`pos` is required")?;
                let state = d
                    .graph
                    .machine_mut(machine)?
                    .states
                    .iter_mut()
                    .find(|s| s.name == name)
                    .ok_or_else(|| format!("no state {name:?}"))?;
                state.pos = pos;
                Ok(())
            }
            other => Err(format!(
                "unknown op {other:?} (add | remove | rename | initial | move)"
            )),
        }
    })
    .map_err(bad)?;
    let e = editor(world);
    let m = e.docs.graph.machine(machine).map_err(bad)?;
    Ok(json!({
        "states": m.states.iter().map(|s| json!({ "name": s.name, "output": s.output })).collect::<Vec<_>>(),
        "initial": m.initial,
    }))
}

fn anim_transition(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let machine = id_param(params, "machine")?;
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("add")
        .to_string();
    let index = params.get("index").and_then(Value::as_u64).map(|v| v as usize);
    let build = || -> Result<TransitionDef, RpcError> {
        let def = json!({
            "from": params.get("from").cloned().unwrap_or(Value::Null),
            "to": str_param(params, "to")?,
            "conditions": params.get("conditions").cloned().unwrap_or_else(|| json!([])),
            "duration": params.get("duration").cloned().unwrap_or_else(|| json!(0.2)),
            "curve": params.get("curve").cloned().unwrap_or_else(|| json!("Smooth")),
            "interruptible": params.get("interruptible").cloned().unwrap_or(Value::Bool(true)),
            "blend": params.get("blend").cloned().unwrap_or_else(|| json!("Crossfade")),
        });
        serde_json::from_value(def).map_err(|e| bad(format!("bad transition: {e}")))
    };
    let result = match op.as_str() {
        "add" => {
            let t = build()?;
            edit(world, "add transition", None, |d| {
                d.graph.add_transition(machine, t)
            })
        }
        "set" => {
            let t = build()?;
            let i = index.ok_or_else(|| bad("`index` is required"))?;
            edit(world, "edit transition", None, |d| {
                d.graph.set_transition(machine, i, t).map(|_| i)
            })
        }
        "remove" => {
            let i = index.ok_or_else(|| bad("`index` is required"))?;
            edit(world, "remove transition", None, |d| {
                d.graph.remove_transition(machine, i).map(|_| i)
            })
        }
        other => return Err(bad(format!("unknown op {other:?} (add | set | remove)"))),
    }
    .map_err(bad)?;
    let e = editor(world);
    let m = e.docs.graph.machine(machine).map_err(bad)?;
    Ok(json!({
        "index": result,
        "transitions": transitions_json(&m.transitions),
    }))
}

/// A machine's transitions as JSON, with `blend` always present (the file format omits the
/// default `Crossfade`; agents should not have to know that).
fn transitions_json(transitions: &[TransitionDef]) -> Value {
    Value::Array(
        transitions
            .iter()
            .map(|t| {
                let mut value = serde_json::to_value(t).unwrap_or(Value::Null);
                if let Value::Object(map) = &mut value {
                    map.insert(
                        "blend".into(),
                        serde_json::to_value(t.blend).unwrap_or(Value::Null),
                    );
                }
                value
            })
            .collect(),
    )
}

fn anim_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| super::graph_panel::agent_path(world, "path", p))
        .transpose()?;
    let mut e = editor(world);
    let unreachable = e.save_graph(path.as_deref()).map_err(bad)?;
    let saved = e.graph_path.clone().unwrap_or_default();
    let bytes = std::fs::metadata(&saved).map(|m| m.len()).unwrap_or(0);
    Ok(json!({ "path": saved.display().to_string(), "bytes": bytes, "unreachable": unreachable }))
}

fn rig_json(e: &AnimEditor) -> Value {
    let Some(rig) = &e.rig else {
        return Value::Null;
    };
    let skeleton = rig.edited_skeleton(&e.docs.edits);
    json!({
        "name": rig.name,
        "joints": skeleton.joint_names,
        "clips": rig.clips.iter().map(|c| json!({ "name": c.name, "duration": c.duration })).collect::<Vec<_>>(),
        "sockets": skeleton.sockets.iter().map(|s| json!({ "name": s.name, "joint": s.joint })).collect::<Vec<_>>(),
        "ik_chains": serde_json::to_value(&e.docs.edits.ik_chains).unwrap_or(Value::Null),
        "sidecar": e.edits_path.as_ref().map(|p| p.display().to_string()),
    })
}

fn anim_rig(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let source = params.get("source").and_then(Value::as_str).unwrap_or("demo");
    let sidecar = params
        .get("sidecar")
        .and_then(Value::as_str)
        .map(|p| super::graph_panel::agent_path(world, "sidecar", p))
        .transpose()?;
    let rig = match source {
        "demo" => PreviewRig::demo(),
        "selected" => {
            let entity = world
                .get_resource::<crate::selection::Selection>()
                .and_then(crate::selection::Selection::primary)
                .ok_or_else(|| bad("nothing is selected"))?;
            super::graph_panel::entity_rig(world, entity)
                .ok_or_else(|| bad("the selection has no SkinnedMesh"))?
        }
        "gltf" => {
            let path = super::graph_panel::agent_path(world, "path", str_param(params, "path")?)?;
            PreviewRig::from_gltf(&path).map_err(bad)?
        }
        other => return Err(bad(format!("unknown source {other:?} (demo | selected | gltf)"))),
    };
    let mut e = editor(world);
    e.set_rig(rig, sidecar).map_err(bad)?;
    Ok(rig_json(&e))
}

fn debug_json(
    debug: &regia_animation::debug::GraphDebug,
    paths: &std::collections::BTreeMap<NodeId, String>,
) -> Value {
    let node_of = |path: &str| paths.iter().find(|(_, p)| p.as_str() == path).map(|(id, _)| *id);
    json!({
        "state": debug.first_state(),
        "nodes": debug.nodes.iter().map(|n| json!({
            "path": n.path,
            "node": node_of(&n.path),
            "active": n.active,
            "weight": n.weight,
            "state": serde_json::to_value(&n.state).unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
        "parameters": debug.parameters.iter().map(|(k, v)| json!({ "name": k, "value": serde_json::to_value(v).unwrap_or(Value::Null) })).collect::<Vec<_>>(),
    })
}

fn anim_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // The preview's speed slider (ADR-7747): it scales every preview step, `advance` included.
    let speed = match params.get("speed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|s| (0.0..=2.0).contains(s))
                .ok_or_else(|| bad("`speed` must be a number in 0..=2 (the preview's speed slider)"))?
                as f32,
        ),
    };
    let mut e = editor(world);
    let e = &mut *e;
    if let Some(speed) = speed {
        e.preview.speed = speed;
    }
    if let Some(Value::Object(values)) = params.get("params") {
        let decls = e.docs.graph.parameters.clone();
        for (name, value) in values {
            let kind = decls
                .iter()
                .find(|d| &d.0 == name)
                .map(|d| d.1)
                .ok_or_else(|| bad(format!("parameter {name:?} is not declared")))?;
            let v = match kind {
                ParamValue::Trigger => ParamValue::Trigger,
                other => {
                    let numbers: Vec<f32> = match value {
                        Value::Array(items) => {
                            items.iter().filter_map(Value::as_f64).map(|v| v as f32).collect()
                        }
                        Value::Number(n) => vec![n.as_f64().unwrap_or(0.0) as f32],
                        _ => Vec::new(),
                    };
                    super::graph_inspector::value_of_kind(&other, &numbers, value.as_bool())
                }
            };
            e.preview.set_param(name, v);
        }
    }
    let advance = params
        .get("advance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 60.0) as f32;
    let rig = e.rig_or_demo().clone();
    let revision = e.revision;
    let steps = (advance * 60.0).ceil().max(1.0) as usize;
    let dt = if advance > 0.0 {
        advance / steps as f32
    } else {
        0.0
    };
    let playing = e.preview.playing;
    e.preview.playing = true;
    for _ in 0..steps {
        e.preview.tick_graph(&rig, &e.docs, revision, dt);
    }
    e.preview.playing = playing;
    let mut out = match &e.preview.debug {
        Some(debug) => debug_json(debug, &e.preview.paths),
        None => json!({ "state": null, "nodes": [], "parameters": [] }),
    };
    out["error"] = json!(e.preview.error);
    out["speed"] = json!(e.preview.speed);
    Ok(out)
}

fn anim_debug(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut e = editor(world);
    // Paths of the open graph (the preview compiles them).
    let rig = e.rig_or_demo().clone();
    let revision = e.revision;
    let e = &mut *e;
    e.preview.tick_graph(&rig, &e.docs, revision, 0.0);
    let Some(pie) = &e.pie_debug else {
        return Ok(Value::Null);
    };
    let mut out = debug_json(&pie.debug, &e.preview.paths);
    out["entity"] = json!(pie.entity);
    out["name"] = json!(pie.name);
    Ok(out)
}

fn recorder_json(recorder: &super::recorder::AnimRecorder) -> Value {
    json!({
        "recording": recorder.is_recording(),
        "frames": recorder.frames().len(),
        "capacity": recorder.capacity(),
        "bytes": recorder.bytes(),
        "dropped": recorder.dropped(),
        "scrub": recorder.scrub(),
        "first": recorder.frames().front().map(|f| f.index),
        "last": recorder.frames().back().map(|f| f.index),
    })
}

fn anim_record(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op")?;
    let mut e = editor(world);
    match op {
        "start" => {
            let capacity = params.get("capacity").and_then(Value::as_u64).unwrap_or(0) as usize;
            e.recorder.start(capacity);
        }
        "stop" => e.recorder.stop(),
        "state" => {}
        other => return Err(bad(format!("unknown op {other:?} (start | stop | state)"))),
    }
    Ok(recorder_json(&e.recorder))
}

fn recorded_frame_json(at: usize, frame: &super::recorder::RecordedFrame) -> Value {
    json!({
        "frame": at,
        "index": frame.index,
        "play_frame": frame.play_frame,
        "entity": frame.entity,
        "name": frame.name,
        "state": frame.state(),
        "transition": frame.transition().map(|(from, to, progress)| json!({ "from": from, "to": to, "progress": progress })),
        "parameters": frame.debug.parameters.iter().map(|(k, v)| json!({ "name": k, "value": serde_json::to_value(v).unwrap_or(Value::Null) })).collect::<Vec<_>>(),
        "weights": frame.blend_weights().iter().map(|(path, w)| json!({ "path": path, "weight": w })).collect::<Vec<_>>(),
        "pose_hash": format!("{:016x}", frame.pose_hash),
    })
}

fn anim_scrub(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frame = match params.get("frame") {
        None => return Err(bad("`frame` is required (a number, or null for live)")),
        Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| RpcError::wrong_type("frame", "a number or null"))? as usize,
        ),
    };
    let mut e = editor(world);
    let e = &mut *e;
    if frame.is_some() && e.recorder.frames().is_empty() {
        return Err(coded(
            "nothing_recorded",
            "nothing is recorded",
            "record during play first: editor.anim_record { op: \"start\" }",
        ));
    }
    e.recorder.set_scrub(frame).map_err(|m| {
        coded(
            "frame_out_of_range",
            m,
            "read editor.anim_record { op: \"state\" } for the number of frames",
        )
    })?;
    match (frame, e.recorder.scrub_frame()) {
        (Some(at), Some(recorded)) => {
            // The graph panel and editor.anim_debug show the recorded frame at once.
            e.pie_debug = Some(recorded.pie_debug());
            Ok(recorded_frame_json(at, recorded))
        }
        _ => Ok(json!({ "frame": Value::Null })),
    }
}

/// `(key, label, edited)` of every curve the panel's multi-curve view lists, in palette order.
fn listed_curves(world: &mut World, panel: &str) -> Result<Vec<(String, String, bool)>, RpcError> {
    match panel {
        "anim_clip" => {
            let mut e = editor(world);
            let e = &mut *e;
            let rig = e.rig_or_demo().clone();
            let clip = e.ui.clip.clone().or_else(|| rig.clip_names().first().cloned());
            let edits = clip
                .as_deref()
                .and_then(|c| e.docs.edits.clip(c))
                .cloned()
                .unwrap_or_default();
            let open = e.ui.curve.clone();
            Ok(edits
                .curves
                .iter()
                .map(|c| (false, c.name.clone()))
                .chain(edits.property_tracks.iter().map(|t| (true, t.path.clone())))
                .map(|(property, name)| {
                    let edited = open.as_ref() == Some(&(property, name.clone()));
                    (super::clip_panel::multi_curve_key(property, &name), name, edited)
                })
                .collect())
        }
        "seq_curve" => {
            let panel = world
                .remove_resource::<crate::seq_curve_panel::SeqCurvePanel>()
                .unwrap_or_default();
            let sequence = crate::seq_curve_panel::player_of(world, &panel)
                .and_then(|p| world.get::<regia_framework::sequencer::SequencePlayer>(p))
                .map(|p| p.sequence().clone());
            let out = sequence
                .map(|s| {
                    crate::seq_curve_panel::sequence_curves(&s)
                        .into_iter()
                        .map(|(t, c, label)| {
                            let edited = panel.track == t && panel.component == c;
                            (crate::seq_curve_panel::seq_curve_key(t, c), label, edited)
                        })
                        .collect()
                })
                .unwrap_or_default();
            world.insert_resource(panel);
            Ok(out)
        }
        other => Err(bad(format!("unknown panel {other:?} (anim_clip | seq_curve)"))),
    }
}

fn curve_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let panel = str_param(params, "panel")?.to_string();
    let listed = listed_curves(world, &panel)?;
    if let Some(key) = params.get("curve").and_then(Value::as_str) {
        if !listed.iter().any(|(k, _, _)| k == key) {
            return Err(coded(
                "unknown_curve",
                format!("the {panel} panel lists no curve {key:?}"),
                "call editor.curve_view { panel } for the keys",
            ));
        }
        let visible = params.get("visible").and_then(Value::as_bool);
        let locked = params.get("locked").and_then(Value::as_bool);
        let apply = |multi: &mut super::curve_view::MultiCurve| {
            if let Some(v) = visible {
                multi.set_visible(key, v);
            }
            if let Some(l) = locked {
                multi.set_locked(key, l);
            }
        };
        if panel == "anim_clip" {
            apply(&mut editor(world).ui.curves);
        } else {
            apply(
                &mut world
                    .get_resource_or_insert_with(crate::seq_curve_panel::SeqCurvePanel::default)
                    .curves,
            );
        }
    }
    let multi = if panel == "anim_clip" {
        editor(world).ui.curves.clone()
    } else {
        world
            .get_resource::<crate::seq_curve_panel::SeqCurvePanel>()
            .map(|p| p.curves.clone())
            .unwrap_or_default()
    };
    let curves: Vec<Value> = listed
        .iter()
        .enumerate()
        .map(|(i, (key, label, edited))| {
            json!({
                "key": key,
                "label": label,
                "palette_index": i,
                "visible": multi.visible(key),
                "locked": multi.is_locked(key),
                "edited": edited,
            })
        })
        .collect();
    Ok(json!({ "panel": panel, "curves": curves }))
}

fn parse_keys(keys: &Value) -> Result<Curve, String> {
    let Value::Array(items) = keys else {
        return Err("`keys` must be an array of [time, value, kind?, slope?]".into());
    };
    let mut out = Vec::new();
    for item in items {
        let a = item
            .as_array()
            .ok_or("each key is [time, value, kind?, slope?]")?;
        let t = a.first().and_then(Value::as_f64).ok_or("key time")? as f32;
        let v = a.get(1).and_then(Value::as_f64).ok_or("key value")? as f32;
        let slope = a.get(3).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let kind = match a.get(2).and_then(Value::as_str).unwrap_or("linear") {
            "constant" => CurveKeyKind::Constant,
            "cubic" => CurveKeyKind::Cubic {
                left_tangent: slope,
                right_tangent: slope,
            },
            _ => CurveKeyKind::Linear,
        };
        out.push(CurveKey::new(t, v, kind));
    }
    Ok(Curve::from(out))
}

/// `editor.anim_clip { op: "list" }`: the rig's clips, filtered and sorted like the picker.
fn anim_clip_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut e = editor(world);
    let e = &mut *e;
    if let Some(filter) = params.get("filter").and_then(Value::as_str) {
        e.ui.clip_list.filter = filter.to_string();
    }
    if let Some(sort) = params.get("sort").and_then(Value::as_str) {
        e.ui.clip_list.sort = super::clip_preview::ClipSort::from_name(sort).ok_or_else(|| {
            bad(format!(
                "unknown sort {sort:?} (imported | name | length | recent)"
            ))
        })?;
    }
    let rig = e.rig_or_demo().clone();
    let clips: Vec<Value> =
        e.ui.clip_list
            .list(&rig)
            .into_iter()
            .map(|name| {
                let duration = rig.clip(&name).map_or(0.0, |c| c.duration);
                json!({ "name": name, "duration": duration })
            })
            .collect();
    Ok(json!({ "clips": clips, "filter": e.ui.clip_list.filter, "sort": e.ui.clip_list.sort.name() }))
}

/// `editor.anim_clip { op: "preview" }`: the clip preview's view state (never undoable).
fn anim_clip_preview(world: &mut World, clip: &str, params: &Value) -> Result<Value, RpcError> {
    use super::clip_preview::{ClipLoop, MAX_GHOSTS, MAX_SPEED, MIN_SPEED};
    let mut e = editor(world);
    let e = &mut *e;
    let imported = e
        .rig_or_demo()
        .clip(clip)
        .map(|c| (**c).clone())
        .ok_or_else(|| bad(format!("the preview rig has no clip {clip:?}")))?;
    let edited = super::edited_clip(&imported, &e.docs.edits);
    if e.ui.clip.as_deref() != Some(clip) {
        e.ui.clip = Some(clip.to_string());
        e.ui.clip_list.picked(clip);
        e.ui.clip_preview.range = None;
    }
    let o = &mut e.ui.clip_preview;
    if let Some(speed) = params.get("speed").and_then(Value::as_f64) {
        o.speed = (speed as f32).clamp(MIN_SPEED, MAX_SPEED);
    }
    if let Some(mode) = params.get("loop").and_then(Value::as_str) {
        o.looping = ClipLoop::from_name(mode)
            .ok_or_else(|| bad(format!("unknown loop mode {mode:?} (once | loop | ping_pong)")))?;
        o.reverse = false;
    }
    match params.get("range") {
        Some(Value::Null) => o.range = None,
        Some(Value::Array(items)) => {
            let ends: Vec<f32> = items.iter().filter_map(Value::as_f64).map(|v| v as f32).collect();
            let [a, b] = ends[..] else {
                return Err(coded(
                    "bad_range",
                    "`range` is [start, end]",
                    "pass two numbers or null",
                ));
            };
            o.set_range(Some([a, b.min(edited.duration)]))
                .map_err(|m| coded("bad_range", m, "pass 0 <= start < end, inside the clip"))?;
        }
        Some(_) => return Err(RpcError::wrong_type("range", "[start, end] or null")),
        None => {}
    }
    if let Some(onion) = params.get("onion") {
        let n = |k: &str| onion.get(k).and_then(Value::as_f64);
        if let Some(v) = onion.get("enabled").and_then(Value::as_bool) {
            o.onion.enabled = v;
        }
        if let Some(v) = n("before") {
            o.onion.before = (v.max(0.0) as u8).min(MAX_GHOSTS);
        }
        if let Some(v) = n("after") {
            o.onion.after = (v.max(0.0) as u8).min(MAX_GHOSTS);
        }
        if let Some(v) = n("step") {
            o.onion.step = (v as f32).clamp(0.01, 2.0);
        }
        if let Some(v) = n("opacity") {
            o.onion.opacity = (v as f32).clamp(0.05, 1.0);
        }
    }
    if let Some(v) = params.get("root_floor").and_then(Value::as_bool) {
        o.root_floor = v;
    }
    let (start, end) = o.range_of(edited.duration);
    if let Some(t) = params.get("time").and_then(Value::as_f64) {
        e.ui.scrub = (t as f32).clamp(start, end);
    }
    let advance = params
        .get("advance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 60.0) as f32;
    if advance > 0.0 {
        let steps = (advance * 60.0).ceil() as usize;
        let dt = advance / steps as f32;
        let mut playing = true;
        for _ in 0..steps {
            e.ui.scrub =
                e.ui.clip_preview
                    .advance(e.ui.scrub, dt, edited.duration, &mut playing);
            if !playing {
                break;
            }
        }
        e.ui.clip_playing = playing;
    }
    let o = &e.ui.clip_preview;
    let (root, yaw) = super::clip_preview::root_at(&edited, start, e.ui.scrub);
    Ok(json!({
        "clip": clip,
        "time": e.ui.scrub,
        "playing": e.ui.clip_playing,
        "speed": o.speed,
        "loop": o.looping.name(),
        "reverse": o.reverse,
        "range": [start, end],
        "trimmed": o.range.is_some(),
        "onion": {
            "enabled": o.onion.enabled,
            "before": o.onion.before,
            "after": o.onion.after,
            "step": o.onion.step,
            "opacity": o.onion.opacity,
        },
        "onion_times": o.onion_times(e.ui.scrub, edited.duration).iter().map(|(k, t)| json!([k, t])).collect::<Vec<_>>(),
        "root_floor": o.root_floor,
        "root": { "position": [root.x, root.y, root.z], "yaw": yaw.to_euler(regia_core::glam::EulerRot::YXZ).0 },
    }))
}

fn anim_clip(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op")?.to_string();
    if op == "list" {
        return anim_clip_list(world, params);
    }
    let clip = str_param(params, "clip")?.to_string();
    if op == "preview" {
        return anim_clip_preview(world, &clip, params);
    }
    let imported = {
        let mut e = editor(world);
        e.rig_or_demo()
            .clip(&clip)
            .map(|c| (**c).clone())
            .ok_or_else(|| bad(format!("the preview rig has no clip {clip:?}")))?
    };
    let index = params.get("index").and_then(Value::as_u64).map(|v| v as usize);
    let time = params.get("time").and_then(Value::as_f64).map(|v| v as f32);
    let name = params.get("name").and_then(Value::as_str).map(str::to_string);
    let payload = params.get("payload").and_then(Value::as_str).map(str::to_string);
    let property = params.get("property").and_then(Value::as_bool).unwrap_or(false);
    let mode = params.get("mode").and_then(Value::as_str).map(str::to_string);
    let keys = params.get("keys").cloned();
    edit(world, &format!("clip {clip} {op}"), None, |d| {
        let edits = super::clip_panel::clip_edits_mut(d, &imported);
        match op.as_str() {
            "add_notify" => {
                edits.notifies.push(Notify {
                    time: time.ok_or("`time` is required")?,
                    name: name.ok_or("`name` is required")?,
                    payload,
                });
                edits.notifies.sort_by(|a, b| a.time.total_cmp(&b.time));
                Ok(())
            }
            "set_notify" => {
                let n = edits
                    .notifies
                    .get_mut(index.ok_or("`index` is required")?)
                    .ok_or("no such notify")?;
                if let Some(t) = time {
                    n.time = t;
                }
                if let Some(name) = name {
                    n.name = name;
                }
                if payload.is_some() {
                    n.payload = payload;
                }
                Ok(())
            }
            "remove_notify" => {
                let i = index.ok_or("`index` is required")?;
                if i >= edits.notifies.len() {
                    return Err(format!("no notify {i}"));
                }
                edits.notifies.remove(i);
                Ok(())
            }
            "root_motion" => {
                edits.root_motion = Some(match mode.as_deref().unwrap_or("XZ") {
                    "None" => RootMotionMode::None,
                    "XZ" => RootMotionMode::XZ,
                    "XYZ" => RootMotionMode::XYZ,
                    "Yaw" => RootMotionMode::Yaw,
                    other => return Err(format!("unknown mode {other:?}")),
                });
                Ok(())
            }
            "set_curve" => {
                let name = name.ok_or("`name` is required")?;
                let curve = parse_keys(keys.as_ref().ok_or("`keys` is required")?)?;
                if property {
                    edits.property_tracks.retain(|t| t.path != name);
                    edits
                        .property_tracks
                        .push(regia_animation::authoring::EditPropertyTrack { path: name, curve });
                } else {
                    edits.curves.retain(|c| c.name != name);
                    edits.curves.push(regia_animation::authoring::EditCurve { name, curve });
                }
                Ok(())
            }
            "remove_curve" => {
                let name = name.ok_or("`name` is required")?;
                edits.curves.retain(|c| property || c.name != name);
                edits.property_tracks.retain(|t| !property || t.path != name);
                Ok(())
            }
            other => Err(format!(
                "unknown op {other:?} (add_notify | set_notify | remove_notify | root_motion | set_curve | remove_curve | preview | list)"
            )),
        }
    })
    .map_err(bad)?;
    let e = editor(world);
    let edited = super::edited_clip(&imported, &e.docs.edits);
    Ok(json!({
        "clip": clip,
        "notifies": serde_json::to_value(&edited.notifies).unwrap_or(Value::Null),
        "curves": edited.curves.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
        "property_tracks": edited.property_tracks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(),
        "root_motion": edited.root_motion.as_ref().map(|r| format!("{:?}", r.mode)),
    }))
}

fn anim_skeleton(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op")?.to_string();
    let name = str_param(params, "name")?.to_string();
    let joint = params.get("joint").and_then(Value::as_str).map(str::to_string);
    let offset = params.get("offset").and_then(Value::as_array).map(|a| {
        let n = |i: usize| a.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        regia_core::math::Vec3::new(n(0), n(1), n(2))
    });
    // The panel's Rotation° fields (ADR-7747): yaw, pitch, roll in degrees (Euler YXZ).
    let rotation = match params.get("rotation") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) if a.len() == 3 && a.iter().all(|v| v.as_f64().is_some_and(f64::is_finite)) => {
            let deg = |i: usize| (a[i].as_f64().unwrap_or(0.0) as f32).to_radians();
            Some(regia_core::math::Quat::from_euler(
                regia_core::math::EulerRot::YXZ,
                deg(0),
                deg(1),
                deg(2),
            ))
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`rotation` must be [yaw, pitch, roll] in degrees (three finite numbers)",
            ));
        }
    };
    let joints: Option<Vec<String>> = params
        .get("joints")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
    let pole = params
        .get("pole_param")
        .and_then(Value::as_str)
        .map(str::to_string);
    let skeleton = {
        let mut e = editor(world);
        e.rig_or_demo().skeleton.clone()
    };
    edit(world, &format!("skeleton {op} {name}"), None, |d| {
        match op.as_str() {
            "add_socket" => {
                let joint = joint.ok_or("`joint` is required")?;
                if skeleton.joint_index(&joint).is_none() {
                    return Err(format!("the rig has no joint {joint:?}"));
                }
                d.edits.sockets.retain(|s| s.name != name);
                d.edits.sockets.push(SkeletonSocket {
                    name,
                    joint,
                    offset: LocalBind {
                        translation: offset.unwrap_or_default(),
                        rotation: rotation.unwrap_or(LocalBind::default().rotation),
                        ..LocalBind::default()
                    },
                });
                Ok(())
            }
            "remove_socket" => {
                let before = d.edits.sockets.len();
                d.edits.sockets.retain(|s| s.name != name);
                if before == d.edits.sockets.len() {
                    Err(format!("no authored socket {name:?}"))
                } else {
                    Ok(())
                }
            }
            "add_ik_chain" => {
                let joints = joints.ok_or("`joints` [root, mid, tip] is required")?;
                let [root, mid, tip]: [String; 3] =
                    joints.try_into().map_err(|_| "`joints` needs exactly 3 names")?;
                let chain = regia_animation::authoring::IkChainDef {
                    name,
                    joints: [root, mid, tip],
                    pole_param: pole,
                };
                if !super::skeleton_panel::valid_chain(&skeleton, &chain) {
                    return Err("the joints are not a parent → child chain in the rig".into());
                }
                d.edits.ik_chains.retain(|c| c.name != chain.name);
                d.edits.ik_chains.push(chain);
                Ok(())
            }
            "remove_ik_chain" => {
                let before = d.edits.ik_chains.len();
                d.edits.ik_chains.retain(|c| c.name != name);
                if before == d.edits.ik_chains.len() {
                    Err(format!("no IK chain {name:?}"))
                } else {
                    Ok(())
                }
            }
            other => Err(format!(
                "unknown op {other:?} (add_socket | remove_socket | add_ik_chain | remove_ik_chain)"
            )),
        }
    })
    .map_err(bad)?;
    let e = editor(world);
    Ok(json!({
        "sockets": serde_json::to_value(&e.docs.edits.sockets).unwrap_or(Value::Null),
        "ik_chains": serde_json::to_value(&e.docs.edits.ik_chains).unwrap_or(Value::Null),
    }))
}

fn anim_save_edits(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| super::graph_panel::agent_path(world, "path", p))
        .transpose()?;
    let saved = editor(world).save_edits(path.as_deref()).map_err(bad)?;
    Ok(json!({ "path": saved.display().to_string() }))
}

/// The timeline the agent drives: the widget's state without a window, so a headless run can
/// exercise scrub, key and marker edits through `editor.timeline_input`.
#[derive(Resource, Debug)]
pub struct TimelineProbe {
    /// The widget state.
    pub timeline: Timeline,
    /// Widget width in points for pointer input.
    pub width: f32,
    /// Edit events since `editor.timeline_state` last drained them.
    pub events: Vec<TimelineEvent>,
}

impl Default for TimelineProbe {
    fn default() -> Self {
        let mut timeline = Timeline::new(1.0);
        timeline.fit(DEFAULT_WIDTH - super::timeline::LABEL_WIDTH);
        Self {
            timeline,
            width: DEFAULT_WIDTH,
            events: Vec::new(),
        }
    }
}

const DEFAULT_WIDTH: f32 = 600.0;

fn probe(world: &mut World) -> Mut<'_, TimelineProbe> {
    if !world.contains_resource::<TimelineProbe>() {
        world.init_resource::<TimelineProbe>();
    }
    world.resource_mut::<TimelineProbe>()
}

fn num(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|v| Some(v as f32))
            .ok_or_else(|| bad(format!("`{name}` must be a number"))),
    }
}

fn selection_json(selection: Option<Selection>) -> Value {
    match selection {
        None => Value::Null,
        Some(Selection::Key { track, index }) => json!({ "key": { "track": track, "index": index } }),
        Some(Selection::Marker(index)) => json!({ "marker": index }),
    }
}

fn event_json(event: &TimelineEvent) -> Value {
    match event {
        TimelineEvent::Scrub(time) => json!({ "event": "scrub", "time": time }),
        TimelineEvent::Selected(s) => json!({ "event": "selected", "selection": selection_json(*s) }),
        TimelineEvent::KeyMoved {
            track,
            index,
            time,
            finished,
        } => {
            json!({ "event": "key_moved", "track": track, "index": index, "time": time, "finished": finished })
        }
        TimelineEvent::KeyAdded { track, time } => {
            json!({ "event": "key_added", "track": track, "time": time })
        }
        TimelineEvent::KeyRemoved { track, index } => {
            json!({ "event": "key_removed", "track": track, "index": index })
        }
        TimelineEvent::MarkerMoved {
            index,
            time,
            finished,
        } => {
            json!({ "event": "marker_moved", "index": index, "time": time, "finished": finished })
        }
        TimelineEvent::MarkerAdded { time, kind } => {
            json!({ "event": "marker_added", "time": time, "kind": kind.name() })
        }
        TimelineEvent::MarkerRemoved { index } => json!({ "event": "marker_removed", "index": index }),
        TimelineEvent::RangeChanged { range, finished } => {
            json!({ "event": "range_changed", "range": range.map(|(a, b)| [a, b]), "finished": finished })
        }
        TimelineEvent::LoopChanged(on) => json!({ "event": "loop_changed", "loop": on }),
    }
}

fn timeline_json(probe: &mut TimelineProbe, drain: bool) -> Value {
    let t = &probe.timeline;
    let events: Vec<Value> = probe.events.iter().map(event_json).collect();
    let out = json!({
        "duration": t.duration,
        "playhead": t.playhead,
        "range": t.range.map(|(a, b)| [a, b]),
        "loop": t.looping,
        "snap": t.snap,
        "selection": selection_json(t.selection),
        "view": { "start": t.view_start, "pixels_per_second": t.pixels_per_second, "width": probe.width },
        "tracks": t.tracks.iter().map(|tr| json!({ "name": tr.name, "keys": tr.keys })).collect::<Vec<_>>(),
        "markers": t.markers.iter().map(|m| json!({ "time": m.time, "label": m.label, "kind": m.kind.name() })).collect::<Vec<_>>(),
        "events": events,
    });
    if drain {
        probe.events.clear();
    }
    out
}

fn timeline_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // Resolve the clip first: it borrows the editor, the probe is a separate resource.
    let loaded = match params.get("clip").and_then(Value::as_str) {
        None => None,
        Some(name) => {
            let mut e = editor(world);
            let rig = e.rig_or_demo().clone();
            let clip = rig
                .clip(name)
                .ok_or_else(|| bad(format!("the preview rig has no clip {name:?}")))?;
            let clip = super::edited_clip(clip, &e.docs.edits);
            Some(Timeline::from_clip(&clip, Some(&*rig.skeleton)))
        }
    };
    let mut p = probe(world);
    let p = &mut *p;
    if let Some(loaded) = loaded {
        let snap = p.timeline.snap;
        p.timeline = loaded;
        p.timeline.snap = snap;
    }
    if let Some(width) = num(params, "width")? {
        p.width = width.clamp(super::timeline::LABEL_WIDTH + 50.0, 8000.0);
    }
    if let Some(duration) = num(params, "duration")? {
        p.timeline.duration = duration.max(0.0);
    }
    if let Some(snap) = num(params, "snap")? {
        p.timeline.snap = (snap > 0.0).then_some(snap);
    } else if params.get("snap") == Some(&Value::Null) {
        p.timeline.snap = None;
    }
    if let Some(looping) = params.get("loop").and_then(Value::as_bool) {
        p.timeline.looping = looping;
    }
    if let Some(tracks) = params.get("tracks") {
        let tracks = tracks
            .as_array()
            .ok_or_else(|| bad("`tracks` must be an array"))?;
        p.timeline.tracks = tracks
            .iter()
            .map(|t| {
                let name = t.get("name").and_then(Value::as_str).unwrap_or("track");
                let keys = t
                    .get("keys")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_f64).map(|v| v as f32).collect())
                    .unwrap_or_default();
                TimelineTrack::new(name, keys)
            })
            .collect();
    }
    if let Some(markers) = params.get("markers") {
        let markers = markers
            .as_array()
            .ok_or_else(|| bad("`markers` must be an array"))?;
        let mut out = Vec::new();
        for m in markers {
            let time = m
                .get("time")
                .and_then(Value::as_f64)
                .ok_or_else(|| bad("a marker needs `time`"))? as f32;
            let label = m.get("label").and_then(Value::as_str).unwrap_or("");
            let kind = match m.get("kind").and_then(Value::as_str) {
                None => MarkerKind::Notify,
                Some(k) => MarkerKind::from_name(k).ok_or_else(|| {
                    bad(format!(
                        "unknown marker kind {k:?} (foot_contact | notify | custom)"
                    ))
                })?,
            };
            out.push(TimelineMarker::new(time, label, kind));
        }
        p.timeline.markers = out;
    }
    match params.get("range") {
        None => {}
        Some(Value::Null) => p.timeline.range = None,
        Some(r) => {
            let a = r
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or_else(|| bad("`range` must be [start, end] or null"))?;
            let (s, e) = (
                a[0].as_f64().ok_or_else(|| bad("`range` must hold numbers"))? as f32,
                a[1].as_f64().ok_or_else(|| bad("`range` must hold numbers"))? as f32,
            );
            p.timeline.range = Some((s.min(e), s.max(e)));
        }
    }
    let duration = p.timeline.duration;
    if let Some(playhead) = num(params, "playhead")? {
        p.timeline.playhead = playhead.clamp(0.0, duration);
    }
    p.timeline.selection = None;
    // Refit when the content or width changed, then apply any zoom.
    let track_width = p.width - super::timeline::LABEL_WIDTH;
    if params.get("clip").is_some() || params.get("duration").is_some() || params.get("width").is_some() {
        p.timeline.fit(track_width);
    }
    if let Some(zoom) = num(params, "zoom")? {
        let anchor = p.timeline.playhead;
        p.timeline.zoom_at(zoom, anchor);
    }
    Ok(timeline_json(p, false))
}

fn timeline_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut p = probe(world);
    Ok(timeline_json(&mut p, true))
}

fn timeline_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use super::timeline::{
        Button, Layout, MARKER_HEIGHT, Pointer, RULER_HEIGHT, TOOLBAR_HEIGHT, TRACK_HEIGHT,
    };
    let op = str_param(params, "op")?.to_string();
    let mut p = probe(world);
    let p = &mut *p;
    let layout = Layout::new(egui::Pos2::ZERO, p.width, &p.timeline);
    let time = num(params, "time")?;
    let row = match params.get("row") {
        None | Some(Value::Null) => "ruler".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(_) => return Err(bad("`row` must be toolbar | ruler | markers | a track index")),
    };
    let y = match row.as_str() {
        "toolbar" => TOOLBAR_HEIGHT * 0.5,
        "ruler" => TOOLBAR_HEIGHT + RULER_HEIGHT * 0.5,
        "markers" => TOOLBAR_HEIGHT + RULER_HEIGHT + MARKER_HEIGHT * 0.5,
        index => {
            let index: usize = index.parse().map_err(|_| {
                bad(format!(
                    "unknown row {index:?} (toolbar | ruler | markers | a track index)"
                ))
            })?;
            if index >= p.timeline.tracks.len() {
                return Err(bad(format!(
                    "track {index} does not exist ({} tracks)",
                    p.timeline.tracks.len()
                )));
            }
            TOOLBAR_HEIGHT + RULER_HEIGHT + MARKER_HEIGHT + (index as f32 + 0.5) * TRACK_HEIGHT
        }
    };
    let pos = || -> Result<egui::Pos2, RpcError> {
        if row == "toolbar" {
            return Ok(egui::Pos2::new(super::timeline::LOOP_BUTTON_WIDTH * 0.5, y));
        }
        let time = time.ok_or_else(|| bad("`time` is required"))?;
        Ok(egui::Pos2::new(p.timeline.x_of(&layout, time), y))
    };
    let input = match op.as_str() {
        "press" => Pointer::Press {
            pos: pos()?,
            button: match params.get("button").and_then(Value::as_str) {
                None | Some("primary") => Button::Primary,
                Some("secondary") => Button::Secondary,
                Some(other) => return Err(bad(format!("unknown button {other:?} (primary | secondary)"))),
            },
            shift: params.get("shift").and_then(Value::as_bool).unwrap_or(false),
        },
        "drag" => Pointer::Drag { pos: pos()? },
        "release" => Pointer::Release {
            pos: pos()?,
            double: params.get("double").and_then(Value::as_bool).unwrap_or(false),
        },
        "delete" => Pointer::Delete,
        other => {
            return Err(bad(format!(
                "unknown op {other:?} (press | drag | release | delete)"
            )));
        }
    };
    let events = p.timeline.pointer(&layout, input);
    let out = json!({
        "events": events.iter().map(event_json).collect::<Vec<_>>(),
        "playhead": p.timeline.playhead,
        "range": p.timeline.range.map(|(a, b)| [a, b]),
        "selection": selection_json(p.timeline.selection),
    });
    p.events.extend(events);
    Ok(out)
}

fn sample_into_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "clip")?.to_string();
    let time = num(params, "time")?.ok_or_else(|| bad("`time` (seconds) is required"))?;
    let mode = match params.get("mode").and_then(Value::as_str) {
        None | Some("replace") => SampleMode::Replace,
        Some("additive") => SampleMode::Additive,
        Some(other) => return Err(bad(format!("unknown mode {other:?} (replace | additive)"))),
    };
    let (clip, skeleton) = {
        let mut e = editor(world);
        let rig = e.rig_or_demo().clone();
        let clip = rig
            .clip(&name)
            .ok_or_else(|| bad(format!("the preview rig has no clip {name:?}")))?;
        (
            super::edited_clip(clip, &e.docs.edits),
            rig.edited_skeleton(&e.docs.edits),
        )
    };
    let skeleton = std::sync::Arc::new(skeleton);
    let binding = spawn_skeleton(world, skeleton.clone());
    let joints = binding.joints.clone();
    let root = world.spawn(binding).id();
    let written = sample_into(world, root, &clip, time, mode);
    let wanted: Option<Vec<String>> = params
        .get("joints")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
    let animated: std::collections::BTreeSet<u32> = clip.tracks.iter().map(|t| t.joint).collect();
    let mut report = Vec::new();
    for (i, joint) in joints.iter().enumerate() {
        let joint_name = &skeleton.joint_names[i];
        let listed = match &wanted {
            Some(names) => names.contains(joint_name),
            None => animated.contains(&(i as u32)),
        };
        if !listed {
            continue;
        }
        if let Some(t) = world.get::<regia_scene::Transform>(*joint) {
            report.push(json!({
                "name": joint_name,
                "translation": t.translation.to_array(),
                "rotation": t.rotation.to_array(),
                "scale": t.scale.to_array(),
            }));
        }
    }
    for joint in joints {
        world.despawn(joint);
    }
    world.despawn(root);
    Ok(json!({
        "clip": name,
        "time": time,
        "mode": if mode == SampleMode::Additive { "additive" } else { "replace" },
        "joint_count": skeleton.len(),
        "written": written,
        "untouched": skeleton.len() - animated.len().min(skeleton.len()),
        "joints": report,
    }))
}

/// A locomotion error with its code and the page hint.
fn locomotion_error(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        code,
        message,
        "see book/agents/animation-editor.md (Locomotion test bed)",
    )
}

/// A finite number, else JSON null (an infinite stability margin means "no support polygon").
fn finite_json(value: f32) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

/// `ground` of `editor.anim_locomotion_preview`: absent is flat; a name takes the preset's
/// defaults; an object overrides them field by field.
fn locomotion_ground(params: &Value) -> Result<super::locomotion_panel::GroundPreset, RpcError> {
    use super::locomotion_panel::GroundPreset;
    let bad = |m: String| locomotion_error("bad_ground", m);
    let (kind, fields) = match params.get("ground") {
        None | Some(Value::Null) => return Ok(GroundPreset::Flat),
        Some(Value::String(name)) => (name.as_str(), None),
        Some(Value::Object(fields)) => (
            fields.get("kind").and_then(Value::as_str).unwrap_or("flat"),
            Some(fields),
        ),
        Some(_) => {
            return Err(bad(
                "`ground` must be a preset name or an object with a `kind`".into()
            ));
        }
    };
    let mut ground = GroundPreset::presets()
        .into_iter()
        .find(|g| g.name() == kind)
        .ok_or_else(|| {
            bad(format!(
                "`ground.kind` is `{kind}`; it must be flat, slope, stairs, rubble or tunnel"
            ))
        })?;
    if let Some(fields) = fields {
        let allowed: &[&str] = match ground {
            GroundPreset::Flat => &["kind"],
            GroundPreset::Slope { .. } => &["kind", "degrees"],
            GroundPreset::Stairs { .. } => &["kind", "rise", "run"],
            GroundPreset::Rubble { .. } => &["kind", "amplitude", "cell", "seed"],
            GroundPreset::Tunnel { .. } => &["kind", "depth"],
        };
        if let Some(key) = fields.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(bad(format!(
                "a {kind} ground has no field `{key}` (fields: {})",
                allowed.join(", ")
            )));
        }
        let number = |key: &str, slot: &mut f32| -> Result<(), RpcError> {
            match fields.get(key) {
                None | Some(Value::Null) => Ok(()),
                Some(v) => {
                    *slot = v
                        .as_f64()
                        .map(|x| x as f32)
                        .ok_or_else(|| bad(format!("`ground.{key}` must be a number")))?;
                    Ok(())
                }
            }
        };
        match &mut ground {
            GroundPreset::Flat => {}
            GroundPreset::Slope { degrees } => number("degrees", degrees)?,
            GroundPreset::Stairs { rise, run } => {
                number("rise", rise)?;
                number("run", run)?;
            }
            GroundPreset::Rubble {
                amplitude,
                cell,
                seed,
            } => {
                number("amplitude", amplitude)?;
                number("cell", cell)?;
                if let Some(v) = fields.get("seed").filter(|v| !v.is_null()) {
                    *seed = v
                        .as_u64()
                        .ok_or_else(|| bad("`ground.seed` must be a nonnegative integer".into()))?;
                }
            }
            GroundPreset::Tunnel { depth } => number("depth", depth)?,
        }
    }
    ground
        .validate()
        .map_err(|m| bad(format!("{kind} ground: {m}")))?;
    Ok(ground)
}

fn ground_json(ground: &super::locomotion_panel::GroundPreset) -> Value {
    use super::locomotion_panel::GroundPreset;
    match *ground {
        GroundPreset::Flat => json!({ "kind": "flat" }),
        GroundPreset::Slope { degrees } => json!({ "kind": "slope", "degrees": degrees }),
        GroundPreset::Stairs { rise, run } => json!({ "kind": "stairs", "rise": rise, "run": run }),
        GroundPreset::Rubble {
            amplitude,
            cell,
            seed,
        } => {
            json!({ "kind": "rubble", "amplitude": amplitude, "cell": cell, "seed": seed })
        }
        GroundPreset::Tunnel { depth } => json!({ "kind": "tunnel", "depth": depth }),
    }
}

fn gait_target(world: &World, params: &Value) -> Result<(Entity, u64, String, std::path::PathBuf), RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::wrong_type("entity", "an entity id (integer)"))?;
    let entity = crate::command::resolve_entity(world, bits).ok_or_else(|| {
        RpcError::new(
            NOT_FOUND,
            "no_such_entity",
            format!("no entity {bits}"),
            "list entities with scene.query_entities",
        )
    })?;
    if world.get::<GaitRig>(entity).is_none() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "no_gait_rig",
            format!("entity {bits} has no GaitRig"),
            "add a GaitRig (book/agents/procedural-gait.md) or call the method on another entity",
        ));
    }
    let rel = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::wrong_type("path", "a project-relative .gait.ron path"))?
        .to_owned();
    let full = super::graph_panel::agent_path(world, "path", &rel)?;
    Ok((entity, bits, rel, full))
}

fn gait_asset_rpc(path: &str, error: GaitAssetError, hint: &'static str) -> RpcError {
    RpcError::new(INVALID_PARAMS, error.code(), format!("{path}: {error}"), hint)
}

/// `editor.anim_locomotion_preview`.
fn anim_locomotion_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use super::locomotion_panel::{
        LocomotionDriver, LocomotionSource, LocomotionTestBed, MAX_PREVIEW_SECONDS, PREVIEW_DT, default_gait,
        entity_gait, gait_rpc_error, steps_for, test_bed_ui,
    };
    let _span = tracing::info_span!(target: "regia_editor", "anim_locomotion_preview").entered();
    if !(params.is_object() || params.is_null()) {
        return Err(bad("the parameters must be an object"));
    }
    let present = |key: &str| params.get(key).filter(|v| !v.is_null());
    let (source, gait) = match (present("entity"), present("asset")) {
        (Some(_), Some(_)) => return Err(bad("pass either `entity` or `asset`, not both")),
        (Some(value), None) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::wrong_type("entity", "an entity id (integer)"))?;
            (LocomotionSource::Entity(bits), entity_gait(world, bits)?)
        }
        (None, Some(value)) => {
            let rel = value
                .as_str()
                .ok_or_else(|| RpcError::wrong_type("asset", "a project-relative .gait.ron path"))?;
            let full = super::graph_panel::agent_path(world, "asset", rel)?;
            let asset = regia_animation::gait_asset::load_gait_file(&full).map_err(|e| {
                RpcError::new(
                    regia_agent::protocol::INVALID_PARAMS,
                    e.code(),
                    format!("asset {rel}: {e}"),
                    "check the file with anim.gait_validate; see book/agents/procedural-gait.md",
                )
            })?;
            (LocomotionSource::Asset(rel.to_owned()), asset.params)
        }
        (None, None) => (LocomotionSource::Default, default_gait()),
    };
    let ground = locomotion_ground(params)?;
    let number = |key: &str, default: f32| -> Result<f32, RpcError> {
        match present(key) {
            None => Ok(default),
            Some(v) => v
                .as_f64()
                .map(|x| x as f32)
                .filter(|x| x.is_finite())
                .ok_or_else(|| locomotion_error("bad_driver", format!("`{key}` must be a finite number"))),
        }
    };
    let driver = LocomotionDriver {
        speed: number("speed", 1.0)?,
        turn_rate: number("turn_rate", 0.0)?,
    };
    driver.validate().map_err(|m| locomotion_error("bad_driver", m))?;
    let seconds = match present("seconds") {
        None => 5.0,
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x > 0.0)
            .ok_or_else(|| RpcError::wrong_type("seconds", "a number above 0"))?,
    };
    if seconds > f64::from(MAX_PREVIEW_SECONDS) {
        return Err(locomotion_error(
            "simulate_too_long",
            format!("`seconds` is {seconds}; at most {MAX_PREVIEW_SECONDS} per call"),
        ));
    }
    let seconds = seconds as f32;
    let screenshot = match present("screenshot") {
        None => None,
        Some(v) => {
            let rel = v
                .as_str()
                .ok_or_else(|| RpcError::wrong_type("screenshot", "a project-relative .png path"))?;
            Some((
                rel.to_owned(),
                super::graph_panel::agent_path(world, "screenshot", rel)?,
            ))
        }
    };
    let mut bed = LocomotionTestBed::new(gait, ground, driver);
    bed.source = source.clone();
    let metrics = bed
        .run(seconds)
        .map_err(|e| gait_rpc_error(e.error, &format!("the walk stopped ({e})")))?;
    let Some(state) = bed.state else {
        return Err(RpcError::internal(
            "the locomotion test bed lost its walker after a run",
        ));
    };
    let shot = match screenshot {
        None => Value::Null,
        Some((rel, full)) => {
            let device = crate::agent_ext::viewport_device(world)?;
            let mut shown = bed.clone();
            let image = super::capture::render_ui_offscreen(&device, [640, 480], 2, |ui| {
                crate::theme::sync(ui.ctx(), world);
                test_bed_ui(ui, &mut shown, 0);
            })
            .map_err(|e| locomotion_error("capture_failed", format!("rendering the test bed: {e}")))?;
            let bytes = crate::screenshot::png_bytes(&image)
                .map_err(|e| locomotion_error("capture_failed", format!("encoding the PNG: {e}")))?;
            crate::atomic_write::write_atomic(&full, &bytes)
                .map_err(|e| locomotion_error("capture_failed", format!("writing {rel}: {e}")))?;
            json!({ "path": rel, "width": image.width(), "height": image.height(), "bytes": bytes.len() })
        }
    };
    let source_json = match &source {
        LocomotionSource::Default => json!("default"),
        LocomotionSource::Entity(bits) => json!({ "entity": bits }),
        LocomotionSource::Asset(rel) => json!({ "asset": rel }),
    };
    let result = json!({
        "source": source_json,
        "ground": ground_json(&ground),
        "speed": driver.speed,
        "turn_rate": driver.turn_rate,
        "seconds": seconds,
        "steps": steps_for(seconds),
        "dt": PREVIEW_DT,
        "legs": state.leg_count(),
        "metrics": {
            "max_slide": metrics.max_slide,
            "max_foot_jump": metrics.max_foot_jump,
            "forced_steps": metrics.forced_steps,
            "refused": metrics.refused,
            "replants": metrics.replants,
            "min_planted": metrics.min_planted,
            "stability_margin": finite_json(metrics.stability_margin),
            "min_stability_margin": finite_json(metrics.min_stability_margin),
            "settled": metrics.settled,
        },
        "final": { "position": state.position, "yaw": state.yaw },
        "screenshot": shot,
    });
    // The panel shows the run the agent asked for.
    if let Some(mut editor) = world.get_resource_mut::<AnimEditor>() {
        editor.locomotion = bed;
    }
    Ok(result)
}

fn anim_gait_load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "anim_gait_load").entered();
    let (entity, bits, rel, full) = gait_target(world, params)?;
    let loaded = super::locomotion_panel::load_gait_into(world, entity, &full).map_err(|error| {
        gait_asset_rpc(
            &rel,
            error,
            "check the file with anim.gait_validate; see book/agents/animation-editor.md",
        )
    })?;
    let rig = world
        .get::<GaitRig>(entity)
        .ok_or_else(|| RpcError::internal("GaitRig disappeared after editor.anim_gait_load"))?;
    Ok(json!({
        "entity": bits,
        "path": rel,
        "loaded": loaded,
        "legs": rig.legs.len(),
        "body": rig.body,
        "auto_hips": rig.auto_hips,
    }))
}

fn anim_gait_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_editor", "anim_gait_save").entered();
    let (entity, bits, rel, full) = gait_target(world, params)?;
    let written = super::locomotion_panel::save_gait_ron(world, entity, &full).map_err(|error| {
        gait_asset_rpc(
            &rel,
            error,
            "fix the rig's settings or choose another path; see book/agents/animation-editor.md",
        )
    })?;
    let rig = world
        .get::<GaitRig>(entity)
        .ok_or_else(|| RpcError::internal("GaitRig disappeared after editor.anim_gait_save"))?;
    Ok(json!({
        "entity": bits,
        "path": rel,
        "written": written,
        "legs": rig.legs.len(),
        "body": rig.body,
        "auto_hips": rig.auto_hips,
    }))
}

fn capture_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = str_param(params, "panel")?;
    let panel = super::capture::AnimPanel::parse(name);
    let history = name == "undo_history";
    let recovery = name == "safe_mode";
    // The Match Rules panel and World Settings (ADR-8738, book/agents/editor-match-rules.md).
    let match_rules = name == "match_rules";
    let world_settings = name == "world_settings";
    if panel.is_none() && !history && !recovery && !match_rules && !world_settings {
        return Err(bad(format!(
            "unknown panel {name:?} (anim_graph | anim_clip | anim_skeleton | undo_history | safe_mode | match_rules | world_settings)"
        )));
    }
    let width = crate::agent_ext::capture_dim(params, "width", 1024);
    let height = crate::agent_ext::capture_dim(params, "height", 640);
    let device = crate::agent_ext::viewport_device(world)?;
    let image = if let Some(panel) = panel {
        super::capture::capture_anim_panel(&device, world, panel, [width, height])
    } else {
        super::capture::render_ui_offscreen(&device, [width, height], 2, |ui| {
            crate::theme::sync(ui.ctx(), world);
            if history {
                crate::history_panel::history_panel(ui, world);
            } else if match_rules {
                crate::match_rules_panel::match_rules_panel(ui, world);
            } else if world_settings {
                crate::world_settings::world_settings_panel(ui, world);
            } else {
                crate::safe_mode::banner(ui, world);
            }
        })
    }
    .map_err(RpcError::internal)?;
    let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
    Ok(json!({ "width": image.width(), "height": image.height(), "mime": "image/png", "png_base64": png }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::gait_rig::{GaitLeg, GaitRig};

    fn world() -> World {
        // The gait load/save methods go through `SetProperty::capture` → `read_field`, which
        // needs the `AppTypeRegistry` the editor app always carries (`command.rs`). Build a real
        // `regia_app::App` (which initialises that resource and the animations' reflected types)
        // and hand its world back to the test, instead of hand-inserting the registry.
        let mut app = regia_app::App::new();
        app.add_plugins(regia_animation::AnimationPlugin);
        app.init_resource::<AnimEditor>();
        app.init_resource::<crate::command::UndoStack>();
        std::mem::take(app.world_mut())
    }

    fn project_world(tag: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets/units")).expect("assets/units");
        let mut world = world();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    fn spawn_gait_rig(world: &mut World) -> Entity {
        let gait = super::super::locomotion_panel::default_gait();
        // The writer refuses a rig whose legs do not cover the pattern's leg count, so name
        // every leg the tripod default asks for.
        let legs = (0..gait.legs())
            .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
            .collect();
        world.spawn((GaitRig::new(gait, legs).with_body("body"),)).id()
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let handler = methods()
            .into_iter()
            .find(|(s, _)| s.name == method)
            .map(|(_, h)| h)
            .unwrap_or_else(|| panic!("no method {method}"));
        handler(world, &params)
    }

    #[test]
    fn gait_asset_methods_save_and_load_a_rig() {
        let (mut world, root) = project_world("anim-gait-agent");
        let entity = spawn_gait_rig(&mut world);
        let bits = entity.to_bits();
        let leg_count = world.get::<GaitRig>(entity).expect("rig").legs.len();
        let saved = call(
            &mut world,
            "editor.anim_gait_save",
            json!({ "entity": bits, "path": "units/walker.gait.ron" }),
        )
        .expect("save");
        assert_eq!(saved["entity"], bits);
        assert_eq!(saved["path"], "units/walker.gait.ron");
        assert!(saved["written"].as_u64().is_some_and(|n| n > 0), "{saved}");
        assert_eq!(saved["legs"], leg_count);
        assert_eq!(saved["body"], "body");
        let path = root.join("assets/units/walker.gait.ron");
        assert!(path.exists(), "{}", path.display());

        {
            let mut rig = world.get_mut::<GaitRig>(entity).expect("rig");
            rig.params.forward_speed = 3.5;
            rig.body = None;
        }
        let loaded = call(
            &mut world,
            "editor.anim_gait_load",
            json!({ "entity": bits, "path": "units/walker.gait.ron" }),
        )
        .expect("load");
        assert_eq!(loaded["entity"], bits);
        assert_eq!(loaded["path"], "units/walker.gait.ron");
        assert_eq!(loaded["loaded"], true, "{loaded}");
        assert_eq!(loaded["legs"], leg_count);
        assert_eq!(loaded["body"], "body");
        let rig = world.get::<GaitRig>(entity).expect("rig");
        assert_eq!(rig.body.as_deref(), Some("body"));
        assert_ne!(rig.params.forward_speed, 3.5);

        assert!(
            crate::command::undo_world(&mut world),
            "the load is one undo step"
        );
        let rig = world.get::<GaitRig>(entity).expect("rig after undo");
        assert_eq!(rig.body, None);
        assert_eq!(rig.params.forward_speed, 3.5);
    }

    #[test]
    fn anim_save_writes_format_version_4() {
        // `.animgraph.ron` v4 is ADR-8475's (ChainIK and BodyAim nodes, c913ea8f6); the save
        // writes the format the runtime reads today, pinned here so the next bump updates this.
        assert_eq!(regia_animation::graph::ANIMGRAPH_FORMAT_VERSION, 4);
        let dir = regia_test_utils::unique_temp_dir("anim-graph-v4");
        let path = dir.join("a.animgraph.ron");
        let mut world = world();
        call(&mut world, "editor.anim_open", json!({ "create": true })).expect("open");
        let node = call(
            &mut world,
            "editor.anim_add_node",
            json!({ "kind": "clip", "data": { "clip": "Walk" } }),
        )
        .expect("node")["node"]
            .as_u64()
            .expect("id");
        call(&mut world, "editor.anim_connect", json!({ "child": node })).expect("output");
        world.resource_mut::<AnimEditor>().graph_path = Some(path.clone());
        call(&mut world, "editor.anim_save", json!({})).expect("save");
        let text = std::fs::read_to_string(&path).expect("saved graph");
        assert!(text.contains("format_version: 4"), "{text}");
    }

    #[test]
    fn capture_panel_width_default_is_inside_the_clamp() {
        let schema = methods()
            .into_iter()
            .find(|(method, _)| method.name == "editor.capture_panel")
            .map(|(method, _)| method)
            .expect("schema");
        let width = schema
            .params
            .iter()
            .find(|param| param.name == "width")
            .expect("width");
        // The schema names the clamp `capture_dim` applies (64..=4096, the offscreen renderer's
        // limit, 91b7e9d93) and the default the handler passes.
        assert!(width.description.contains("1024"), "{}", width.description);
        assert!(width.description.contains("4096"), "{}", width.description);
        assert!(!width.description.contains("1100"), "{}", width.description);
        assert_eq!(crate::agent_ext::capture_dim(&json!({}), "width", 1024), 1024);
        assert_eq!(
            crate::agent_ext::capture_dim(&json!({ "width": 1100 }), "width", 1024),
            1100
        );
        assert_eq!(
            crate::agent_ext::capture_dim(&json!({ "width": 5000 }), "width", 1024),
            4096
        );
    }

    #[test]
    fn node_data_merges_and_errors_are_clean() {
        let mut world = world();
        call(&mut world, "editor.anim_open", json!({ "create": true })).expect("open");
        call(&mut world, "editor.anim_param", json!({ "name": "Speed" })).expect("param");
        let node = call(
            &mut world,
            "editor.anim_add_node",
            json!({ "kind": "clip", "data": { "clip": "Run", "speed": 1.5 } }),
        )
        .expect("add")["node"]
            .as_u64()
            .expect("id");
        let set = call(
            &mut world,
            "editor.anim_set_node",
            json!({ "node": node, "data": { "looping": false } }),
        )
        .expect("set");
        assert_eq!(set["data"]["clip"], "Run", "merge keeps other fields");
        assert_eq!(set["data"]["looping"], false);
        assert!(call(&mut world, "editor.anim_add_node", json!({ "kind": "warp" })).is_err());
        assert!(
            call(
                &mut world,
                "editor.anim_set_node",
                json!({ "node": node, "data": { "speed": "fast" } })
            )
            .is_err()
        );
        call(&mut world, "editor.anim_connect", json!({ "child": node })).expect("output");
        let graph = call(&mut world, "editor.anim_graph", json!({ "ron": true })).expect("graph");
        assert_eq!(graph["valid"], true, "{graph}");
        assert!(graph["ron"].as_str().is_some_and(|r| r.contains("Run")));
        let preview = call(
            &mut world,
            "editor.anim_preview",
            json!({ "params": { "Speed": 2.0 }, "advance": 0.5 }),
        )
        .expect("preview");
        assert!(preview["error"].is_null(), "{preview}");
        assert!(preview["nodes"].as_array().is_some_and(|n| !n.is_empty()));
        assert!(
            call(
                &mut world,
                "editor.anim_preview",
                json!({ "params": { "Nope": 1 } })
            )
            .is_err()
        );
        // Undo reverts the last edit (the output connection).
        assert!(crate::command::undo_world(&mut world));
        let graph = call(&mut world, "editor.anim_graph", json!({})).expect("graph");
        assert_eq!(graph["valid"], false);
        assert!(
            call(&mut world, "editor.anim_debug", json!({}))
                .expect("debug")
                .is_null()
        );
    }

    #[test]
    fn timeline_methods_edit_and_report() {
        let mut world = world();
        call(
            &mut world,
            "editor.timeline_set",
            json!({
                "duration": 2.0, "snap": 0.1, "loop": true,
                "tracks": [{ "name": "hips", "keys": [0.0, 1.0] }],
                "markers": [{ "time": 0.5, "label": "plant", "kind": "foot_contact" }],
            }),
        )
        .expect("set");
        let r = call(
            &mut world,
            "editor.timeline_input",
            json!({ "op": "press", "row": "ruler", "time": 0.74 }),
        )
        .expect("scrub");
        assert_eq!(r["events"][0]["event"], "scrub");
        assert!(
            (r["playhead"].as_f64().expect("number") - 0.7).abs() < 1e-5,
            "{r}"
        );
        call(
            &mut world,
            "editor.timeline_input",
            json!({ "op": "release", "row": "ruler", "time": 0.74 }),
        )
        .expect("up");

        call(
            &mut world,
            "editor.timeline_input",
            json!({ "op": "press", "row": 0, "time": 1.0 }),
        )
        .expect("grab key");
        let r = call(
            &mut world,
            "editor.timeline_input",
            json!({ "op": "release", "row": 0, "time": 1.5 }),
        )
        .expect("drop key");
        assert_eq!(r["events"][0]["event"], "key_moved");
        assert_eq!(r["events"][0]["finished"], true);

        let state = call(&mut world, "editor.timeline_state", json!({})).expect("state");
        assert_eq!(state["tracks"][0]["keys"], json!([0.0, 1.5]));
        assert_eq!(state["markers"][0]["kind"], "foot_contact");
        assert_eq!(state["loop"], true);
        assert!(
            state["events"].as_array().is_some_and(|e| e.len() >= 3),
            "{state}"
        );
        let again = call(&mut world, "editor.timeline_state", json!({})).expect("state");
        assert_eq!(again["events"], json!([]), "events drain");

        assert!(
            call(
                &mut world,
                "editor.timeline_input",
                json!({ "op": "press", "row": 9, "time": 0.0 })
            )
            .is_err()
        );
        assert!(call(&mut world, "editor.timeline_input", json!({ "op": "wobble" })).is_err());
        assert!(
            call(
                &mut world,
                "editor.timeline_set",
                json!({ "markers": [{ "time": 0.0, "kind": "x" }] })
            )
            .is_err()
        );
    }

    #[test]
    fn timeline_loads_a_rig_clip_and_sample_into_reports_joints() {
        let mut world = world();
        let clips = {
            let mut e = editor(&mut world);
            e.rig_or_demo().clip_names()
        };
        let clip = clips.first().expect("demo clips").clone();
        let r = call(&mut world, "editor.timeline_set", json!({ "clip": clip })).expect("load");
        assert!(!r["tracks"].as_array().expect("tracks").is_empty(), "{r}");
        let a = call(
            &mut world,
            "editor.sample_into",
            json!({ "clip": clip, "time": 0.0 }),
        )
        .expect("sample");
        let b = call(
            &mut world,
            "editor.sample_into",
            json!({ "clip": clip, "time": 0.4, "mode": "replace" }),
        )
        .expect("sample");
        assert_eq!(a["written"], b["written"]);
        assert!(a["written"].as_u64().expect("number") > 0);
        assert_ne!(a["joints"], b["joints"], "the pose moves with time");
        assert!(
            call(
                &mut world,
                "editor.sample_into",
                json!({ "clip": "nope", "time": 0.0 })
            )
            .is_err()
        );
        assert!(
            call(
                &mut world,
                "editor.sample_into",
                json!({ "clip": clip, "time": 0.0, "mode": "x" })
            )
            .is_err()
        );
    }

    #[test]
    fn clip_and_skeleton_authoring() {
        let mut world = world();
        call(&mut world, "editor.anim_rig", json!({ "source": "demo" })).expect("rig");
        let out = call(
            &mut world,
            "editor.anim_clip",
            json!({ "clip": "Walk", "op": "add_notify", "time": 0.5, "name": "Breath" }),
        )
        .expect("notify");
        assert_eq!(out["notifies"].as_array().map(Vec::len), Some(3));
        let out = call(
            &mut world,
            "editor.anim_clip",
            json!({ "clip": "Walk", "op": "set_curve", "name": "FootPlant_L", "keys": [[0.0, 1.0], [0.5, 0.0, "cubic", 0.0]] }),
        )
        .expect("curve");
        assert_eq!(out["curves"], json!(["FootPlant_L"]));
        assert!(
            call(
                &mut world,
                "editor.anim_clip",
                json!({ "clip": "Nope", "op": "add_notify" })
            )
            .is_err()
        );
        call(
            &mut world,
            "editor.anim_skeleton",
            json!({ "op": "add_socket", "name": "weapon", "joint": "hand_r", "offset": [0.0, -0.05, 0.0] }),
        )
        .expect("socket");
        assert!(
            call(
                &mut world,
                "editor.anim_skeleton",
                json!({ "op": "add_socket", "name": "x", "joint": "tail" })
            )
            .is_err()
        );
        let out = call(
            &mut world,
            "editor.anim_skeleton",
            json!({ "op": "add_ik_chain", "name": "LegL", "joints": ["thigh_l", "shin_l", "foot_l"] }),
        )
        .expect("chain");
        assert_eq!(out["ik_chains"][0]["name"], "LegL");
        assert!(
            call(
                &mut world,
                "editor.anim_skeleton",
                json!({ "op": "add_ik_chain", "name": "Bad", "joints": ["head", "shin_l", "foot_l"] })
            )
            .is_err()
        );
    }

    /// P-208: `editor.curve_view` lists, hides and locks the curves of both multi-curve views.
    #[test]
    fn curve_view_lists_hides_and_locks_curves_of_both_panels() {
        use regia_framework::sequencer::{
            Channel, Ease, ScalarKey, Sequence, SequencePlayer, Track, Vec3Key,
        };
        let mut world = world();
        call(&mut world, "editor.anim_rig", json!({ "source": "demo" })).expect("rig");
        for name in ["Lean", "Bob"] {
            call(
                &mut world,
                "editor.anim_clip",
                json!({ "clip": "Walk", "op": "set_curve", "name": name, "keys": [[0.0, 0.0], [1.0, 1.0]] }),
            )
            .expect("curve");
        }
        world.resource_mut::<AnimEditor>().ui.clip = Some("Walk".into());
        let out = call(&mut world, "editor.curve_view", json!({ "panel": "anim_clip" })).expect("list");
        let keys: Vec<&str> = out["curves"]
            .as_array()
            .expect("curves")
            .iter()
            .filter_map(|c| c["key"].as_str())
            .collect();
        assert_eq!(keys, ["curve:Lean", "curve:Bob"]);
        let out = call(
            &mut world,
            "editor.curve_view",
            json!({ "panel": "anim_clip", "curve": "curve:Bob", "visible": false, "locked": true }),
        )
        .expect("set");
        assert_eq!(out["curves"][1]["visible"], false, "{out}");
        assert_eq!(out["curves"][1]["locked"], true, "{out}");
        assert!(!world.resource::<AnimEditor>().ui.curves.visible("curve:Bob"));
        let err = call(
            &mut world,
            "editor.curve_view",
            json!({ "panel": "anim_clip", "curve": "curve:Nope", "visible": false }),
        )
        .expect_err("unknown");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("unknown_curve")
        );
        assert!(call(&mut world, "editor.curve_view", json!({ "panel": "graph" })).is_err());

        let sequence = Sequence {
            tracks: vec![
                Track {
                    name: "fade".into(),
                    target: "ui".into(),
                    property: "opacity".into(),
                    keys: Channel::Float(vec![
                        ScalarKey::new(0.0, 0.0, Ease::Linear),
                        ScalarKey::new(1.0, 1.0, Ease::Linear),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "move".into(),
                    target: "hero".into(),
                    property: "transform.position".into(),
                    keys: Channel::Vec3(vec![Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Linear)]),
                    enabled: true,
                    locked: false,
                },
            ],
            ..Sequence::default()
        };
        world.spawn(SequencePlayer::new(sequence));
        let out = call(
            &mut world,
            "editor.curve_view",
            json!({ "panel": "seq_curve", "curve": "track:1:2", "locked": true }),
        )
        .expect("seq");
        let labels: Vec<&str> = out["curves"]
            .as_array()
            .expect("curves")
            .iter()
            .filter_map(|c| c["label"].as_str())
            .collect();
        assert_eq!(labels, ["fade", "move.x", "move.y", "move.z"]);
        assert_eq!(out["curves"][0]["edited"], true, "{out}");
        assert_eq!(out["curves"][3]["locked"], true, "{out}");
        assert!(
            world
                .resource::<crate::seq_curve_panel::SeqCurvePanel>()
                .curves
                .is_locked("track:1:2")
        );
    }
}
