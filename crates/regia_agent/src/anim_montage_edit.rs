// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for montage authoring (spec 34 M8 and M10, ADR-1870,
//! `book/agents/montages.md`): `anim.montage_describe` reads a `.montage.ron` (or a child
//! montage, `.montagevar.ron`, resolved against its parent), `anim.montage_edit` applies one
//! edit atomically and keeps an undo stack per asset, `anim.montage_slots` reads a skeleton's
//! slot table (`.slots.ron`), `anim.montage_slot_edit` adds, changes, renames or deletes a slot
//! (rename rewrites every referencing montage and graph `Slot` node, all or none), and
//! `anim.montage_control` reverses or time-stretches a playing montage.
//!
//! Paths are project-relative (no `..`, no absolute paths); files are written atomically
//! (temp file + rename). Errors carry a lowercase `data.error_code`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regia_animation::montage::{FollowMode, Montage, SectionNext, SegmentFollow};
use regia_animation::montage_edit::{EditReport, EditedItem, MontageEdit, MontageEditError, VariantEdit};
use regia_animation::{
    AnimGraph, AnimatorSource, MontageId, MontagePlayer, MontageVariant, SkeletonSlots, SlotEntry, SlotsError,
};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::anim_montage::{invalid, missing, rig_of};
use crate::protocol::RpcError;

/// Undo texts kept per asset.
const UNDO_DEPTH: usize = 16;

/// Most directory entries a slot rename or delete scans for referencing files.
const SCAN_LIMIT: usize = 20_000;

const MONTAGE_SUFFIX: &str = ".montage.ron";
const VARIANT_SUFFIX: &str = ".montagevar.ron";
const SLOTS_SUFFIX: &str = ".slots.ron";
const GRAPH_SUFFIX: &str = ".animgraph.ron";

/// Earlier texts of montage assets `anim.montage_edit` wrote (`edit: { op: "undo" }`).
#[derive(Resource, Default)]
pub struct MontageEditHistory {
    stacks: HashMap<String, Vec<String>>,
}

impl MontageEditHistory {
    /// How many undo steps `asset` has.
    #[must_use]
    pub fn depth(&self, asset: &str) -> usize {
        self.stacks.get(asset).map_or(0, Vec::len)
    }

    fn push(&mut self, asset: &str, text: String) {
        let stack = self.stacks.entry(asset.to_string()).or_default();
        if stack.len() == UNDO_DEPTH {
            stack.remove(0);
        }
        stack.push(text);
    }

    fn pop(&mut self, asset: &str) -> Option<String> {
        self.stacks.get_mut(asset)?.pop()
    }

    /// Drop `asset`'s steps: something other than `anim.montage_edit` rewrote the file, and an
    /// older text would undo that too.
    fn forget(&mut self, asset: &str) {
        self.stacks.remove(asset);
    }
}

/// The shared request-path root (ADR-8798): the asset server's root, then
/// `AgentSettings::project_dir`, then the working directory only when neither is set.
fn project_root(world: &World) -> PathBuf {
    crate::req_path::project_root(world)
}

/// A project-relative path parameter with one of `suffixes`, normalized to `/` separators.
/// `check_relative` accepts `\` as a separator, so the raw text would name `anim\a.ron` (one
/// file name) on Linux and `anim/a.ron` on Windows (ADR-5597); every host now gets the latter.
fn asset_param(params: &Value, key: &str, suffixes: &[&str]) -> Result<String, RpcError> {
    let path = crate::params::opt_str(params, key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            invalid(
                "bad_path",
                format!(
                    "`{key}` must be a project-relative {} path",
                    suffixes.join(" or ")
                ),
            )
        })?;
    let confined = regia_core::project_path::check_relative(path).is_ok();
    let lower = path.to_ascii_lowercase();
    if !confined || !suffixes.iter().any(|s| lower.ends_with(s)) {
        return Err(invalid(
            "bad_path",
            format!(
                "`{key}` must be a relative {} path inside the project (no `..`, not absolute)",
                suffixes.join(" or ")
            ),
        ));
    }
    Ok(path
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect::<Vec<_>>()
        .join("/"))
}

/// `root/relative` resolved and checked to stay inside `root` (symlinks included).
fn inside(root: &Path, relative: &str) -> Result<PathBuf, RpcError> {
    let unknown = |e: std::io::Error| {
        missing(
            "montage_unknown_asset",
            format!("{relative}: {e}"),
            "paths are relative to the project root",
        )
    };
    let root = root.canonicalize().map_err(unknown)?;
    let full = root.join(relative);
    // The file may not exist yet (a new slot table): check its directory.
    let (resolved, check) = match full.canonicalize() {
        Ok(path) => (path.clone(), path),
        Err(_) => {
            // ADR-10066: a dangling symlink at the file itself cannot be checked, so it is refused
            // (the shared write rule, book/agents/safety.md), not replaced on trust.
            if regia_core::project_path::check_under_root(&root, &full).is_err() {
                return Err(invalid(
                    "bad_path",
                    format!("{relative} resolves outside the project (symlink)"),
                ));
            }
            let parent = full
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.clone());
            let parent = parent.canonicalize().map_err(unknown)?;
            let name = full.file_name().map(PathBuf::from).unwrap_or_default();
            (parent.join(name), parent)
        }
    };
    if !check.starts_with(&root) {
        return Err(invalid(
            "bad_path",
            format!("{relative} resolves outside the project (symlink)"),
        ));
    }
    Ok(resolved)
}

fn read_text(root: &Path, relative: &str) -> Result<String, RpcError> {
    let path = inside(root, relative)?;
    std::fs::read_to_string(&path).map_err(|e| {
        missing(
            "montage_unknown_asset",
            format!("{relative}: {e}"),
            "paths are relative to the project root",
        )
    })
}

/// Write through a temp file beside the target and a rename.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = temp_beside(path);
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

fn write_error(relative: &str, e: &std::io::Error) -> RpcError {
    RpcError::internal(format!("{relative}: cannot write: {e}"))
}

/// Clip lengths (seconds) the running app knows: every montage player's and animator's clips.
fn clip_lengths(world: &mut World) -> HashMap<String, f32> {
    let mut out = HashMap::new();
    let mut players = world.query::<&MontagePlayer>();
    for player in players.iter(world) {
        for name in player.clips.names() {
            if let Some(clip) = player.clips.get(name) {
                out.insert(name.to_string(), clip.duration);
            }
        }
    }
    let mut animators = world.query::<&regia_animation::Animator>();
    for animator in animators.iter(world) {
        if let AnimatorSource::Graph { clips, .. } = &animator.source {
            for name in clips.names() {
                if let Some(clip) = clips.get(name) {
                    out.entry(name.to_string()).or_insert(clip.duration);
                }
            }
        }
    }
    out
}

fn follow_json(follow: Option<&SegmentFollow>) -> Value {
    follow.map_or(Value::Null, |f| {
        json!({
            "track": f.track,
            "segment": f.segment,
            "mode": match f.mode {
                FollowMode::KeepTime => "keep_time",
                FollowMode::ShiftWithStart => "shift_with_start",
                FollowMode::ScaleWithLength => "scale_with_length",
            },
        })
    })
}

fn next_json(next: &SectionNext) -> Value {
    match next {
        SectionNext::Continue => json!("continue"),
        SectionNext::Stop => json!("stop"),
        SectionNext::Jump(to) => json!({ "jump": to }),
    }
}

/// The full description of a montage: segments placed on the timeline where the clip lengths
/// are known, sections, notifies, warps, blends, slots and issues.
fn describe_json(montage: &Montage, lengths: &HashMap<String, f32>, slots: Option<&SkeletonSlots>) -> Value {
    let mut length: Option<f32> = Some(0.0);
    let tracks: Vec<Value> = montage
        .tracks
        .iter()
        .enumerate()
        .map(|(ti, track)| {
            let mut at = Some(0.0_f32);
            let segments: Vec<Value> = track
                .segments
                .iter()
                .enumerate()
                .map(|(si, seg)| {
                    let clip = lengths.get(&seg.clip).copied();
                    let end = seg.end.or(clip);
                    let len = end.map(|end| {
                        let end = clip.map_or(end, |d| end.min(d));
                        (end - seg.start).max(0.0) / seg.rate * seg.loops as f32
                    });
                    let here = at;
                    at = at.zip(len).map(|(a, l)| a + l);
                    json!({
                        "index": si,
                        "clip": seg.clip,
                        "start": seg.start,
                        "end": seg.end,
                        "rate": seg.rate,
                        "loops": seg.loops,
                        "clip_length": clip,
                        "at": here,
                        "length": len,
                    })
                })
                .collect();
            length = match (length, at) {
                (Some(l), Some(a)) => Some(l.max(a)),
                _ => None,
            };
            json!({ "index": ti, "slot": track.slot, "segments": segments })
        })
        .collect();
    let mut issues: Vec<Value> = Vec::new();
    if let Some(slots) = slots {
        for issue in montage.validate_against(slots) {
            issues.push(json!({ "code": issue.code, "track": issue.track, "detail": issue.detail }));
        }
    }
    for track in &montage.tracks {
        for seg in &track.segments {
            if seg.end.is_none() && !lengths.contains_key(&seg.clip) {
                issues.push(json!({
                    "code": "montage_clip_length_unknown",
                    "detail": format!("clip {:?} plays to its end and is not loaded; timeline positions after it are unknown", seg.clip),
                }));
            }
        }
    }
    json!({
        "name": montage.name,
        "format_version": montage.format_version,
        "rate": montage.rate,
        "length": length,
        "tracks": tracks,
        "slots_used": montage.tracks.iter().map(|t| t.slot.clone()).collect::<Vec<_>>(),
        "sections": montage.sections.iter().map(|s| json!({
            "name": s.name,
            "start": s.start,
            "next": next_json(&s.next),
            "follow": follow_json(s.follow.as_ref()),
        })).collect::<Vec<_>>(),
        "notifies": montage.notifies.iter().map(|n| json!({
            "name": n.name,
            "time": n.time,
            "duration": n.duration,
            "branch": n.branch,
            "payload": n.payload,
            "follow": follow_json(n.follow.as_ref()),
        })).collect::<Vec<_>>(),
        "warps": montage.warps.iter().enumerate().map(|(i, w)| json!({
            "index": i,
            "target": w.target,
            "start": w.start,
            "end": w.end,
            "translation": w.translation,
            "vertical": w.vertical,
            "rotation": w.rotation,
            "mode": format!("{:?}", w.mode),
            "rotation_kind": format!("{:?}", w.rotation_kind),
            "rotation_fraction": w.rotation_fraction,
            "anchor": format!("{:?}", w.anchor),
        })).collect::<Vec<_>>(),
        "blend": {
            "blend_in": montage.blend_in.time,
            "blend_out": montage.blend_out.time,
            "blend_out_lead": montage.blend_out_lead,
            "auto_blend_out": montage.auto_blend_out,
        },
        "root_motion": montage.root_motion,
        "sync_group": montage.sync_group,
        "rate_curve": montage.rate_curve.as_ref().map(|c| json!({
            "keys": c.keys,
            "interpolation": format!("{:?}", c.interpolation).to_lowercase(),
        })),
        "issues": issues,
    })
}

fn parse_montage(text: &str, asset: &str) -> Result<Montage, RpcError> {
    Montage::from_ron_str(text).map_err(|e| invalid("montage_invalid", format!("{asset}: {e}")))
}

/// A child montage's parent file: `parent` when it names a `.montage.ron`, else
/// `<the variant's directory>/<parent>.montage.ron`.
fn parent_path(variant_asset: &str, parent: &str) -> String {
    if parent.to_ascii_lowercase().ends_with(MONTAGE_SUFFIX) {
        return parent.to_string();
    }
    match Path::new(variant_asset)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        Some(dir) => format!("{}/{parent}{MONTAGE_SUFFIX}", dir.to_string_lossy()),
        None => format!("{parent}{MONTAGE_SUFFIX}"),
    }
}

fn load_variant(root: &Path, asset: &str) -> Result<(MontageVariant, String, Montage), RpcError> {
    let text = read_text(root, asset)?;
    let variant = MontageVariant::from_ron_str(&text)
        .map_err(|e| invalid("montage_invalid", format!("{asset}: {e}")))?;
    let parent_asset = parent_path(asset, &variant.parent);
    if regia_core::project_path::check_relative(&parent_asset).is_err() {
        return Err(invalid(
            "bad_path",
            format!("{asset}: parent {:?} must be project-relative", variant.parent),
        ));
    }
    let parent = parse_montage(&read_text(root, &parent_asset)?, &parent_asset)?;
    Ok((variant, parent_asset, parent))
}

fn slots_of(world: &World, params: &Value) -> Result<Option<SkeletonSlots>, RpcError> {
    if params.get("slots").is_none_or(Value::is_null) {
        return Ok(None);
    }
    let path = asset_param(params, "slots", &[SLOTS_SUFFIX])?;
    let path = path.as_str();
    let text = read_text(&project_root(world), path)?;
    SkeletonSlots::from_ron_str(&text)
        .map(Some)
        .map_err(|e| invalid("montage_invalid", format!("{path}: {e}")))
}

/// `anim.montage_describe { asset, slots? }`: a montage (or child montage) with its timeline,
/// links and issues.
pub fn montage_describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let asset = asset_param(params, "asset", &[MONTAGE_SUFFIX, VARIANT_SUFFIX])?;
    let asset = asset.as_str();
    let root = project_root(world);
    let slots = slots_of(world, params)?;
    let lengths = clip_lengths(world);
    if asset.to_ascii_lowercase().ends_with(VARIANT_SUFFIX) {
        let (variant, parent_asset, parent) = load_variant(&root, asset)?;
        let lookup = |c: &str| lengths.get(c).copied();
        let resolved = variant
            .resolve(&parent, &lookup)
            .map_err(|e| invalid("montage_invalid", format!("{asset}: {e}")))?;
        let mut out = describe_json(&resolved, &lengths, slots.as_ref());
        // A `rescale: false` override can shorten the timeline under inherited items: they are
        // reported, not moved (spec 34 M4, ADR-7216).
        if let Some(end) = resolved.timeline_length(&lookup)
            && let Some(issues) = out.get_mut("issues").and_then(Value::as_array_mut)
        {
            for item in resolved.items_past_end(end) {
                issues.push(json!({
                    "code": item.code,
                    "kind": item.kind,
                    "index": item.index,
                    "name": item.name,
                    "time": item.time,
                    "end": item.end,
                    "detail": format!(
                        "{} {} {:?} at {} s lies past the variant's end {} s",
                        item.kind, item.index, item.name, item.time, item.end
                    ),
                }));
            }
        }
        if let Some(object) = out.as_object_mut() {
            object.insert("asset".into(), json!(asset));
            object.insert("kind".into(), json!("variant"));
            object.insert("variant_of".into(), json!(parent_asset));
            object.insert(
                "overrides".into(),
                json!(
                    variant
                        .overrides
                        .iter()
                        .map(|o| json!({
                            "track": o.track, "segment": o.segment, "clip": o.clip, "rescale": o.rescale,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        return Ok(out);
    }
    let montage = parse_montage(&read_text(&root, asset)?, asset)?;
    let mut out = describe_json(&montage, &lengths, slots.as_ref());
    if let Some(object) = out.as_object_mut() {
        object.insert("asset".into(), json!(asset));
        object.insert("kind".into(), json!("montage"));
        object.insert("variant_of".into(), Value::Null);
    }
    Ok(out)
}

fn edit_error(error: &MontageEditError) -> RpcError {
    invalid(error.code(), error.to_string())
}

fn report_json(report: &EditReport) -> Value {
    json!({
        "moved": report.moved.iter().map(|m| json!({
            "kind": kind_str(m.kind), "index": m.index, "name": m.name, "from": m.from, "to": m.to,
        })).collect::<Vec<_>>(),
        "unlinked": report.unlinked.iter().map(|(k, n)| json!({ "kind": kind_str(*k), "name": n })).collect::<Vec<_>>(),
    })
}

fn kind_str(kind: EditedItem) -> &'static str {
    match kind {
        EditedItem::Section => "section",
        EditedItem::Notify => "notify",
    }
}

fn bool_flag(params: &Value, key: &str) -> Result<bool, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid("bad_bool", format!("`{key}` must be a boolean"))),
    }
}

/// `anim.montage_edit { asset, edit, dry_run? }`: one atomic, validated, undoable edit.
pub fn montage_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let asset = asset_param(params, "asset", &[MONTAGE_SUFFIX, VARIANT_SUFFIX])?;
    let asset = asset.as_str();
    let edit = params
        .get("edit")
        .filter(|e| e.is_object())
        .ok_or_else(|| invalid("montage_bad_edit", "`edit` must be an object with an `op`"))?;
    let dry_run = bool_flag(params, "dry_run")?;
    let root = project_root(world);
    let path = inside(&root, asset)?;
    let before = read_text(&root, asset)?;
    let op = crate::params::opt_str(edit, "op")?.unwrap_or_default();
    world.init_resource::<MontageEditHistory>();
    if op == "undo" {
        let mut history = world.resource_mut::<MontageEditHistory>();
        let Some(previous) = history.pop(asset) else {
            return Err(invalid("montage_bad_edit", format!("{asset}: nothing to undo")));
        };
        if dry_run {
            history.push(asset, previous);
            return Ok(
                json!({ "asset": asset, "op": "undo", "saved": false, "undo_depth": history.depth(asset) }),
            );
        }
        let depth = history.depth(asset);
        if let Err(e) = write_atomic(&path, &previous) {
            history.push(asset, previous);
            return Err(write_error(asset, &e));
        }
        return Ok(json!({ "asset": asset, "op": "undo", "saved": true, "undo_depth": depth }));
    }
    let lengths = clip_lengths(world);
    let lookup = |c: &str| lengths.get(c).copied();
    let (text, report, described) = if asset.to_ascii_lowercase().ends_with(VARIANT_SUFFIX) {
        let edit: VariantEdit = serde_json::from_value(edit.clone()).map_err(|e| {
            invalid(
                "montage_bad_edit",
                format!("a child montage takes only set_override and remove_override: {e}"),
            )
        })?;
        let (mut variant, _, parent) = load_variant(&root, asset)?;
        edit.apply(&mut variant, &parent).map_err(|e| edit_error(&e))?;
        variant
            .validate()
            .map_err(|e| invalid("montage_invalid", e.to_string()))?;
        let resolved = variant
            .resolve(&parent, &lookup)
            .map_err(|e| invalid("montage_invalid", e.to_string()))?;
        let text = variant
            .to_ron_string()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        (
            text,
            EditReport::default(),
            describe_json(&resolved, &lengths, None),
        )
    } else {
        let edit: MontageEdit = serde_json::from_value(edit.clone())
            .map_err(|e| invalid("montage_bad_edit", format!("`edit`: {e}")))?;
        let mut montage = parse_montage(&before, asset)?;
        let report = montage.apply_edit(&edit, &lookup).map_err(|e| edit_error(&e))?;
        let text = montage
            .to_ron_string()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let described = describe_json(&montage, &lengths, None);
        (text, report, described)
    };
    let mut saved = false;
    if !dry_run {
        write_atomic(&path, &text).map_err(|e| write_error(asset, &e))?;
        world.resource_mut::<MontageEditHistory>().push(asset, before);
        saved = true;
    }
    let depth = world.resource::<MontageEditHistory>().depth(asset);
    let mut out = report_json(&report);
    if let Some(object) = out.as_object_mut() {
        object.insert("asset".into(), json!(asset));
        object.insert("op".into(), json!(op));
        object.insert("saved".into(), json!(saved));
        object.insert("undo_depth".into(), json!(depth));
        object.insert("montage".into(), described);
    }
    Ok(out)
}

fn slot_json(slot: &SlotEntry) -> Value {
    json!({
        "name": slot.name,
        "group": slot.group,
        "mask": slot.mask.as_ref().map(|m| json!({ "joints": m.joints, "include_children": m.include_children })),
    })
}

/// The slot table at `path`, or the engine default when the file does not exist.
fn read_slots(root: &Path, path: &str) -> Result<(SkeletonSlots, bool), RpcError> {
    let full = inside(root, path)?;
    if !full.exists() {
        return Ok((SkeletonSlots::default(), true));
    }
    let text = read_text(root, path)?;
    SkeletonSlots::from_ron_str(&text)
        .map(|t| (t, false))
        .map_err(|e| invalid("montage_invalid", format!("{path}: {e}")))
}

/// Every file with `suffix` under `root` (skipping hidden directories and `target`),
/// project-relative, sorted.
fn scan_project(root: &Path, suffix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > SCAN_LIMIT {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(path);
                }
            } else if kind.is_file()
                && name.to_ascii_lowercase().ends_with(suffix)
                && let Ok(relative) = path.strip_prefix(root)
            {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

/// The paths of list parameter `key`, or every file with `suffix` in the project.
fn scan_list(root: &Path, params: &Value, key: &str, suffix: &'static str) -> Result<Vec<String>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(scan_project(root, suffix)),
        Some(Value::Array(items)) => {
            let mut list = Vec::new();
            for item in items {
                let one = json!({ "p": item });
                list.push(asset_param(&one, "p", &[suffix])?);
            }
            Ok(list)
        }
        Some(_) => Err(invalid(
            "bad_path",
            format!("`{key}` must be a list of {suffix} paths"),
        )),
    }
}

/// What a project scan found: the `(path, parsed)` files that name a slot, and the paths of the
/// files that could not be read as this kind of asset.
type SlotScan<T> = (Vec<(String, T)>, Vec<String>);

/// Montages naming `slot`: the `montages` list when given, else a scan of the project. Each is
/// `(path, parsed)`; files that do not parse are returned in the second list.
fn referencing(root: &Path, slot: &str, params: &Value) -> Result<SlotScan<Montage>, RpcError> {
    let candidates = scan_list(root, params, "montages", MONTAGE_SUFFIX)?;
    let quoted = format!("\"{slot}\"");
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for path in candidates {
        let Ok(text) = read_text(root, &path) else {
            unreadable.push(path);
            continue;
        };
        if !text.contains(&quoted) {
            continue;
        }
        match Montage::from_ron_str(&text) {
            Ok(m) if m.tracks.iter().any(|t| t.slot == slot) => found.push((path, m)),
            Ok(_) => {}
            Err(_) => unreadable.push(path),
        }
    }
    Ok((found, unreadable))
}

/// Every anim graph with a `Slot` node (the `graphs` list when given, else a scan of the
/// project), with its slot names; the files that name `Slot` nodes but do not parse are returned
/// in the second list.
fn slot_graphs(root: &Path, params: &Value) -> Result<SlotScan<AnimGraph>, RpcError> {
    let mut found = Vec::new();
    let mut unparsed = Vec::new();
    for path in scan_list(root, params, "graphs", GRAPH_SUFFIX)? {
        let Ok(text) = read_text(root, &path) else {
            continue;
        };
        if !text.contains("Slot(") {
            continue;
        }
        match AnimGraph::from_ron_str(&text) {
            Ok(graph) if graph.has_slot_nodes() => found.push((path, graph)),
            Ok(_) => {}
            Err(_) => unparsed.push(path),
        }
    }
    Ok((found, unparsed))
}

/// `anim.montage_slots { path, montages?, graphs? }`: a skeleton's slot table, the montages and
/// graph `Slot` nodes using each slot, and the graph nodes naming a slot the table lacks.
pub fn montage_slots(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = asset_param(params, "path", &[SLOTS_SUFFIX])?;
    let path = path.as_str();
    let root = project_root(world);
    let (table, default) = read_slots(&root, path)?;
    let mut used_by = serde_json::Map::new();
    for slot in &table.slots {
        let (found, _) = referencing(&root, &slot.name, params)?;
        used_by.insert(
            slot.name.clone(),
            json!(found.into_iter().map(|(p, _)| p).collect::<Vec<_>>()),
        );
    }
    let (graphs, _) = slot_graphs(&root, params)?;
    let mut graphs_by_slot = serde_json::Map::new();
    let mut graph_issues = Vec::new();
    for (graph_path, graph) in &graphs {
        for name in graph.slot_names() {
            if table.get(&name).is_some() {
                if let Some(list) = graphs_by_slot
                    .entry(name)
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                {
                    list.push(json!(graph_path));
                }
            } else {
                graph_issues
                    .push(json!({ "code": "animgraph_unknown_slot", "graph": graph_path, "slot": name }));
            }
        }
    }
    Ok(json!({
        "path": path,
        "default": default,
        "slots": table.slots.iter().map(slot_json).collect::<Vec<_>>(),
        "groups": table.groups(),
        "used_by": used_by,
        "used_by_graphs": graphs_by_slot,
        "graph_issues": graph_issues,
    }))
}

fn slots_error(error: &SlotsError) -> RpcError {
    match error {
        SlotsError::UnknownSlot(_) => missing(
            "slot_unknown",
            error.to_string(),
            "list the table with anim.montage_slots",
        ),
        SlotsError::InUse { .. } => invalid("slot_in_use", error.to_string()),
        _ => invalid("montage_invalid", error.to_string()),
    }
}

fn mask_param(op: &Value) -> Result<Option<Option<regia_animation::graph::JointMaskDef>>, RpcError> {
    if bool_flag(op, "full_body")? {
        return Ok(Some(None));
    }
    match op.get("joints") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let joints: Option<Vec<String>> = items.iter().map(|j| j.as_str().map(str::to_string)).collect();
            let joints = joints
                .ok_or_else(|| invalid("montage_bad_edit", "`joints` must be a list of joint names"))?;
            let include_children = match op.get("include_children") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(invalid("bad_bool", "`include_children` must be a boolean")),
            };
            Ok(Some(Some(regia_animation::graph::JointMaskDef {
                joints,
                include_children,
            })))
        }
        Some(_) => Err(invalid(
            "montage_bad_edit",
            "`joints` must be a list of joint names",
        )),
    }
}

fn str_field<'a>(op: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    crate::params::opt_str(op, key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            invalid(
                "montage_bad_edit",
                format!("`op.{key}` must be a non-empty string"),
            )
        })
}

/// The bytes and permissions of a file `commit_all` replaced, for the rollback restore.
type StagedOriginal = (Vec<u8>, std::fs::Permissions);

/// Write every `(target, text)`, all or none: every temp file first, then the renames; a rename
/// that fails puts back the files already replaced.
fn commit_all(targets: &[(PathBuf, &str)]) -> Result<(), RpcError> {
    let mut staged: Vec<(PathBuf, &Path, Option<StagedOriginal>)> = Vec::new();
    let discard = |staged: &[(PathBuf, &Path, Option<StagedOriginal>)]| {
        for (tmp, _, _) in staged {
            let _ = std::fs::remove_file(tmp);
        }
    };
    for (target, text) in targets {
        let tmp = temp_beside(target);
        // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
        if let Err(e) = crate::req_path::write_new_temp(&tmp, text.as_bytes()) {
            discard(&staged);
            let _ = std::fs::remove_file(&tmp);
            return Err(write_error(&target.to_string_lossy(), &e));
        }
        let original = std::fs::read(target)
            .ok()
            .and_then(|bytes| std::fs::metadata(target).ok().map(|m| (bytes, m.permissions())));
        staged.push((tmp, target, original));
    }
    for (done, (tmp, target, _)) in staged.iter().enumerate() {
        if let Err(e) = std::fs::rename(tmp, target) {
            for (_, replaced, original) in &staged[..done] {
                match original {
                    Some((bytes, perms)) => {
                        // ADR-10066: the restore goes through a fresh temp and a rename too, so
                        // it never writes through a link at either name. The temp is created
                        // with default permissions: put the original attributes (read-only
                        // bit, Unix mode) on it before it replaces the file.
                        let restore = temp_beside(replaced);
                        if crate::req_path::write_new_temp(&restore, bytes).is_ok() {
                            let _ = std::fs::set_permissions(&restore, perms.clone());
                            if std::fs::rename(&restore, replaced).is_err() {
                                let _ = std::fs::remove_file(&restore);
                            }
                        }
                    }
                    None => {
                        let _ = std::fs::remove_file(replaced);
                    }
                }
            }
            discard(&staged[done..]);
            return Err(write_error(&target.to_string_lossy(), &e));
        }
    }
    Ok(())
}

/// `anim.montage_slot_edit { path, op: { op: add | set | rename | delete, ... }, force?,
/// montages?, graphs? }`.
pub fn montage_slot_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = asset_param(params, "path", &[SLOTS_SUFFIX])?;
    let path = path.as_str();
    let op = params
        .get("op")
        .filter(|o| o.is_object())
        .ok_or_else(|| invalid("montage_bad_edit", "`op` must be an object with an `op` name"))?;
    let name = crate::params::opt_str(op, "op")?.unwrap_or_default();
    let force = bool_flag(params, "force")?;
    let root = project_root(world);
    let slots_path = inside(&root, path)?;
    let (mut table, _) = read_slots(&root, path)?;
    let mut rewritten: Vec<(String, String)> = Vec::new();
    // Files that may name the slot but could not be parsed: left alone and reported.
    let mut unparsed: Vec<String> = Vec::new();
    match name {
        "add" => {
            let slot = str_field(op, "name")?;
            if table.get(slot).is_some() {
                return Err(invalid(
                    "montage_bad_edit",
                    format!("a slot named {slot:?} exists"),
                ));
            }
            let group = crate::params::opt_str(op, "group")?.unwrap_or("default");
            table.slots.push(SlotEntry {
                name: slot.to_string(),
                group: group.to_string(),
                mask: mask_param(op)?.flatten(),
            });
        }
        "set" => {
            let slot = str_field(op, "name")?;
            let mask = mask_param(op)?;
            let entry = table.slots.iter_mut().find(|s| s.name == slot).ok_or_else(|| {
                missing(
                    "slot_unknown",
                    format!("no slot named {slot:?}"),
                    "list the table with anim.montage_slots",
                )
            })?;
            if let Some(group) = crate::params::opt_str(op, "group")? {
                entry.group = group.to_string();
            }
            if let Some(mask) = mask {
                entry.mask = mask;
            }
        }
        "rename" => {
            let from = str_field(op, "from")?;
            let to = str_field(op, "to")?;
            let (found, bad_montages) = referencing(&root, from, params)?;
            let (mut graphs, bad_graphs) = slot_graphs(&root, params)?;
            unparsed.extend(bad_montages.into_iter().chain(bad_graphs));
            graphs.retain(|(_, g)| g.slot_names().iter().any(|n| n == from));
            let (paths, mut montages): (Vec<String>, Vec<Montage>) = found.into_iter().unzip();
            table
                .rename(from, to, &mut montages)
                .map_err(|e| slots_error(&e))?;
            for (p, m) in paths.into_iter().zip(montages) {
                let text = m.to_ron_string().map_err(|e| RpcError::internal(e.to_string()))?;
                rewritten.push((p, text));
            }
            for (p, mut g) in graphs {
                g.rename_slot(from, to);
                let text = g.to_ron_string().map_err(|e| RpcError::internal(e.to_string()))?;
                rewritten.push((p, text));
            }
        }
        "delete" => {
            let slot = str_field(op, "name")?;
            let (found, bad_montages) = referencing(&root, slot, params)?;
            let (graphs, bad_graphs) = slot_graphs(&root, params)?;
            unparsed.extend(bad_montages.into_iter().chain(bad_graphs));
            let graph_users = graphs
                .iter()
                .filter(|(_, g)| g.slot_names().iter().any(|n| n == slot))
                .map(|(p, _)| p.as_str());
            let files: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).chain(graph_users).collect();
            if !force && table.get(slot).is_some() && !files.is_empty() {
                return Err(invalid(
                    "slot_in_use",
                    format!("slot {slot:?} is named by {files:?}; pass force: true to delete it anyway"),
                ));
            }
            table.delete(slot, true, &[]).map_err(|e| slots_error(&e))?;
        }
        other => {
            return Err(invalid(
                "montage_bad_edit",
                format!("`op.op` {other:?} is not add, set, rename or delete"),
            ));
        }
    }
    table.validate().map_err(|e| slots_error(&e))?;
    let slots_text = table
        .to_ron_string()
        .map_err(|e| RpcError::internal(e.to_string()))?;
    // A read-only target refuses the whole edit on every host (all or none): on Unix a
    // rename over a read-only file would succeed and silently drop the bit, while Windows
    // would fail mid-commit with a raw OS error. Every op rewrites the table; a rename
    // additionally rewrites every referencing montage and graph.
    let read_only: Vec<&str> = rewritten
        .iter()
        .map(|(p, _)| p.as_str())
        .chain(std::iter::once(path))
        .filter(|p| {
            inside(&root, p)
                .ok()
                .and_then(|full| std::fs::metadata(full).ok())
                .is_some_and(|m| m.permissions().readonly())
        })
        .collect();
    if !read_only.is_empty() {
        return Err(invalid(
            "montage_read_only",
            format!("read-only files refuse the edit: {read_only:?}; nothing changed"),
        ));
    }
    let mut targets: Vec<(PathBuf, &str)> = Vec::new();
    for (p, text) in &rewritten {
        targets.push((inside(&root, p)?, text.as_str()));
    }
    targets.push((slots_path, slots_text.as_str()));
    commit_all(&targets)?;
    // An older text of a rewritten montage would bring the old slot name back.
    if let Some(mut history) = world.get_resource_mut::<MontageEditHistory>() {
        for (p, _) in &rewritten {
            history.forget(p);
        }
    }
    Ok(json!({
        "path": path,
        "op": name,
        "slots": table.slots.iter().map(slot_json).collect::<Vec<_>>(),
        "rewritten": rewritten.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
        "unparsed": unparsed,
    }))
}

/// `anim.montage_control { entity, id?, reverse?, duration? }`: turn a playing montage around
/// or time-stretch the rest of it (ADR-1870).
pub fn montage_control(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let id = match params.get("id") {
        None | Some(Value::Null) => None,
        Some(v) => Some(MontageId(v.as_u64().ok_or_else(|| {
            invalid("bad_id", "`id` must be a montage id (from anim.montage_state)")
        })?)),
    };
    let reverse = match params.get("reverse") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(invalid("bad_bool", "`reverse` must be a boolean")),
    };
    let duration = match params.get("duration") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_f64() {
            Some(d) if d.is_finite() && d > 0.0 => Some(d as f32),
            _ => {
                return Err(invalid(
                    "bad_rate",
                    "`duration` must be a number of wall seconds > 0",
                ));
            }
        },
    };
    if reverse.is_none() && duration.is_none() {
        return Err(invalid("montage_bad_edit", "pass `reverse` and/or `duration`"));
    }
    let mut player = world.get_mut::<MontagePlayer>(entity).ok_or_else(|| {
        missing(
            "no_montage_player",
            format!("entity {} has no MontagePlayer", entity.to_bits()),
            "play a montage with anim.montage_play first",
        )
    })?;
    let unknown = || {
        let which = match id {
            Some(id) => format!("montage id {}", id.0),
            None => "the montage".to_string(),
        };
        missing(
            "montage_not_playing",
            format!("{which} is not playing or queued"),
            "list instances with anim.montage_state",
        )
    };
    if let Some(reverse) = reverse
        && !player.set_reverse(id, reverse)
    {
        return Err(unknown());
    }
    if let Some(seconds) = duration
        && !player.stretch_to(id, seconds)
    {
        return Err(unknown());
    }
    let status = match id {
        Some(id) => player.status(id),
        None => player.instances().last().cloned(),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "instance": status.map(|s| json!({
            "id": s.id.0, "montage": s.montage, "time": s.time, "rate": s.rate, "reverse": s.reverse,
        })),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMBO: &str = r#"(format_version: 2, name: "combo", tracks: [(slot: "full", segments: [
        (clip: "a", end: Some(1.0)), (clip: "b", end: Some(1.0))])],
        sections: [(name: "windup", start: 0.0), (name: "swing", start: 1.0,
            follow: Some((track: 0, segment: 1, mode: ShiftWithStart)))])"#;

    fn world_at(root: &Path) -> World {
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: regia_asset_pipeline::host_platform().to_string(),
            content_dir: None,
        })
        .expect("opens");
        world.insert_resource(server);
        world
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("error_code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn describe_edit_and_undo_a_montage_file() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-edit");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        std::fs::write(root.join("anim/combo.montage.ron"), COMBO).expect("write");
        let mut world = world_at(&root);
        let described =
            montage_describe(&mut world, &json!({ "asset": "anim/combo.montage.ron" })).expect("describe");
        assert_eq!(described["kind"], "montage");
        assert_eq!(described["length"], 2.0);
        assert_eq!(described["tracks"][0]["segments"][1]["at"], 1.0);
        assert_eq!(described["sections"][1]["follow"]["mode"], "shift_with_start");
        // Trim segment 0 to 0.5 s: the linked section follows segment 1 to 0.5.
        let edited = montage_edit(
            &mut world,
            &json!({ "asset": "anim/combo.montage.ron",
                     "edit": { "op": "move_segment", "track": 0, "segment": 0, "end": 0.5 } }),
        )
        .expect("edit");
        assert_eq!(edited["saved"], true);
        assert_eq!(edited["moved"][0]["name"], "swing");
        assert_eq!(edited["moved"][0]["to"], 0.5);
        assert_eq!(edited["undo_depth"], 1);
        let on_disk = std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read");
        let montage = Montage::from_ron_str(&on_disk).expect("valid");
        assert_eq!(montage.sections[1].start, 0.5);
        // A dry run changes nothing on disk.
        let dry = montage_edit(
            &mut world,
            &json!({ "asset": "anim/combo.montage.ron", "dry_run": true,
                     "edit": { "op": "add_notify", "name": "hit", "time": 0.25 } }),
        )
        .expect("dry");
        assert_eq!(dry["saved"], false);
        assert_eq!(
            std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read"),
            on_disk
        );
        // Undo restores the original text.
        montage_edit(
            &mut world,
            &json!({ "asset": "anim/combo.montage.ron", "edit": { "op": "undo" } }),
        )
        .expect("undo");
        assert_eq!(
            std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read"),
            COMBO
        );
        let nothing = montage_edit(
            &mut world,
            &json!({ "asset": "anim/combo.montage.ron", "edit": { "op": "undo" } }),
        )
        .expect_err("empty stack");
        assert_eq!(code(&nothing), "montage_bad_edit");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn edit_errors_carry_codes_and_change_nothing() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-edit-errors");
        std::fs::write(root.join("combo.montage.ron"), COMBO).expect("write");
        let mut world = world_at(&root);
        let cases = [
            (
                json!({ "asset": "../combo.montage.ron", "edit": { "op": "undo" } }),
                "bad_path",
            ),
            (
                json!({ "asset": "/abs/combo.montage.ron", "edit": { "op": "undo" } }),
                "bad_path",
            ),
            (
                json!({ "asset": "nope.montage.ron", "edit": { "op": "remove_warp", "index": 0 } }),
                "montage_unknown_asset",
            ),
            (
                json!({ "asset": "combo.montage.ron", "edit": { "op": "teleport" } }),
                "montage_bad_edit",
            ),
            (
                json!({ "asset": "combo.montage.ron", "edit": { "op": "remove_segment", "track": 3, "segment": 0 } }),
                "montage_bad_edit",
            ),
            // Swing would land on windup's start: sections must stay strictly increasing.
            (
                json!({ "asset": "combo.montage.ron", "edit": { "op": "move_segment", "track": 0, "segment": 0, "end": 0.0 } }),
                "montage_invalid",
            ),
        ];
        for (params, expected) in cases {
            let err = montage_edit(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), expected, "{params}");
        }
        assert_eq!(
            std::fs::read_to_string(root.join("combo.montage.ron")).expect("read"),
            COMBO
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn child_montage_describes_through_its_parent_and_edits_only_overrides() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-variant");
        std::fs::write(root.join("combo.montage.ron"), COMBO).expect("write");
        std::fs::write(
            root.join("combo_axe.montagevar.ron"),
            r#"(format_version: 1, name: "combo_axe", parent: "combo", overrides: [(track: 0, segment: 1, clip: "axe")])"#,
        )
        .expect("write");
        let mut world = world_at(&root);
        let d =
            montage_describe(&mut world, &json!({ "asset": "combo_axe.montagevar.ron" })).expect("describe");
        assert_eq!(d["kind"], "variant");
        assert_eq!(d["variant_of"], "combo.montage.ron");
        assert_eq!(d["tracks"][0]["segments"][1]["clip"], "axe");
        let refused = montage_edit(
            &mut world,
            &json!({ "asset": "combo_axe.montagevar.ron", "edit": { "op": "add_notify", "name": "x", "time": 0.0 } }),
        )
        .expect_err("read-only timing");
        assert_eq!(code(&refused), "montage_bad_edit");
        montage_edit(
            &mut world,
            &json!({ "asset": "combo_axe.montagevar.ron", "edit": { "op": "set_override", "track": 0, "segment": 0, "clip": "axe_windup" } }),
        )
        .expect("override");
        let back = MontageVariant::from_ron_str(
            &std::fs::read_to_string(root.join("combo_axe.montagevar.ron")).expect("read"),
        )
        .expect("valid");
        assert_eq!(back.overrides.len(), 2);
        std::fs::remove_dir_all(root).ok();
    }

    /// Spec 34 M4 (ADR-7216): a `rescale: false` child montage shorter than its parent lists the
    /// inherited section and notify that now lie past its end as `montage_past_end` issues.
    #[test]
    fn a_shorter_child_montage_lists_items_past_its_end() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-past-end");
        std::fs::write(
            root.join("lunge.montage.ron"),
            r#"(format_version: 2, name: "lunge", tracks: [(slot: "full", segments: [(clip: "long")])],
                sections: [(name: "windup", start: 0.0), (name: "strike", start: 0.6)],
                notifies: [(name: "hit", time: 0.8)])"#,
        )
        .expect("write");
        std::fs::write(
            root.join("lunge_quick.montagevar.ron"),
            r#"(format_version: 1, name: "lunge_quick", parent: "lunge", overrides: [(track: 0, segment: 0, clip: "short")])"#,
        )
        .expect("write");
        let mut world = world_at(&root);
        let mut clips = regia_animation::ClipLibrary::new();
        clips.insert(regia_animation::AnimationClip::empty("long", 1.0));
        clips.insert(regia_animation::AnimationClip::empty("short", 0.5));
        world.spawn(MontagePlayer::new(Vec::new(), clips));
        let d = montage_describe(&mut world, &json!({ "asset": "lunge_quick.montagevar.ron" }))
            .expect("describe");
        let past: Vec<(String, String)> = d["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter(|i| i["code"] == "montage_past_end")
            .map(|i| {
                (
                    i["kind"].as_str().unwrap_or_default().to_string(),
                    i["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert_eq!(
            past,
            vec![
                ("section".to_string(), "strike".to_string()),
                ("notify".to_string(), "hit".to_string())
            ],
            "{d}"
        );
        let parent =
            montage_describe(&mut world, &json!({ "asset": "lunge.montage.ron" })).expect("describe");
        assert!(
            parent["issues"]
                .as_array()
                .is_some_and(|issues| issues.iter().all(|i| i["code"] != "montage_past_end")),
            "{parent}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn slot_table_rename_rewrites_montages_all_or_none_and_delete_lists_users() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-slots");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        std::fs::write(root.join("anim/combo.montage.ron"), COMBO).expect("write");
        std::fs::write(
            root.join("anim/wave.montage.ron"),
            r#"(format_version: 2, name: "wave", tracks: [(slot: "full", segments: [(clip: "w")])])"#,
        )
        .expect("write");
        let mut world = world_at(&root);
        // No file yet: the engine default table.
        let read = montage_slots(&mut world, &json!({ "path": "hero.slots.ron" })).expect("slots");
        assert_eq!(read["default"], true);
        montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "add", "name": "full", "group": "body" } }),
        )
        .expect("add");
        let read = montage_slots(&mut world, &json!({ "path": "hero.slots.ron" })).expect("slots");
        assert_eq!(read["default"], false);
        assert_eq!(
            read["used_by"]["full"],
            json!(["anim/combo.montage.ron", "anim/wave.montage.ron"])
        );
        // Delete without force lists the users.
        let err = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "delete", "name": "full" } }),
        )
        .expect_err("in use");
        assert_eq!(code(&err), "slot_in_use");
        assert!(err.message.contains("anim/wave.montage.ron"), "{}", err.message);
        // A read-only referencing montage refuses the rename; nothing changes.
        let wave = root.join("anim/wave.montage.ron");
        let writable = std::fs::metadata(&wave).expect("meta").permissions();
        let mut perms = writable.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&wave, perms).expect("readonly");
        let err = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "rename", "from": "full", "to": "body" } }),
        )
        .expect_err("read-only");
        assert_eq!(code(&err), "montage_read_only");
        assert_eq!(
            std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read"),
            COMBO
        );
        std::fs::set_permissions(&wave, writable).expect("writable");
        let renamed = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "rename", "from": "full", "to": "body" } }),
        )
        .expect("rename");
        assert_eq!(renamed["rewritten"].as_array().map(Vec::len), Some(2));
        for file in ["anim/combo.montage.ron", "anim/wave.montage.ron"] {
            let m = Montage::from_ron_str(&std::fs::read_to_string(root.join(file)).expect("read"))
                .expect("valid");
            assert_eq!(m.tracks[0].slot, "body", "{file}");
        }
        let unknown = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "rename", "from": "full", "to": "x" } }),
        )
        .expect_err("gone");
        assert_eq!(code(&unknown), "slot_unknown");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn slot_edit_refuses_read_only_targets_on_every_host() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-slots-readonly");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        std::fs::write(root.join("anim/combo.montage.ron"), COMBO).expect("write");
        let mut world = world_at(&root);
        montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "add", "name": "full", "group": "body" } }),
        )
        .expect("add");
        montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "add", "name": "extra" } }),
        )
        .expect("add");
        let table = root.join("hero.slots.ron");
        let writable = std::fs::metadata(&table).expect("meta").permissions();
        let mut perms = writable.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&table, perms).expect("readonly");
        let before = std::fs::read_to_string(&table).expect("read");
        // Every op refuses a read-only table with the stable code: on Unix a rename over a
        // read-only file would otherwise succeed and silently drop the bit, while Windows
        // fails mid-commit with a raw OS error.
        let cases = [
            (json!({ "op": "set", "name": "full", "group": "arms" }), "set"),
            (json!({ "op": "add", "name": "extra2" }), "add"),
            (
                json!({ "op": "delete", "name": "extra", "force": true }),
                "delete",
            ),
            (json!({ "op": "rename", "from": "full", "to": "body" }), "rename"),
        ];
        for (op, what) in cases {
            let err = montage_slot_edit(&mut world, &json!({ "path": "hero.slots.ron", "op": op }))
                .expect_err("read-only table");
            assert_eq!(code(&err), "montage_read_only", "{what}");
            assert!(err.message.contains("hero.slots.ron"), "{what}: {}", err.message);
            assert_eq!(std::fs::read_to_string(&table).expect("read"), before, "{what}");
            assert!(
                std::fs::metadata(&table).expect("meta").permissions().readonly(),
                "{what}: the refusal must not touch the file"
            );
            // A rename refusal leaves the writable referencing montage alone too.
            assert_eq!(
                std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read"),
                COMBO,
                "{what}"
            );
        }
        // Clearing the bit makes the same request succeed, on every host.
        std::fs::set_permissions(&table, writable).expect("writable");
        let done = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "set", "name": "full", "group": "arms" } }),
        )
        .expect("set");
        let full = done["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|s| s["name"] == "full")
            .expect("the full slot");
        assert_eq!(full["group"], "arms");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn commit_all_rollback_restores_bytes_and_permissions() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-commit-rollback");
        let a = root.join("a.slots.ron");
        let b = root.join("b.slots.ron");
        std::fs::write(&a, "A").expect("write");
        std::fs::write(&b, "B").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o711)).expect("mode");
        }
        // The third target is a directory: the rename fails on every OS after the first two
        // targets were replaced, forcing the rollback path.
        let blocker = root.join("c.slots.ron");
        std::fs::create_dir(&blocker).expect("dir");
        let targets = [(a.clone(), "A2"), (b.clone(), "B2"), (blocker.clone(), "C")];
        let err = commit_all(&targets).expect_err("third rename fails");
        assert!(err.message.contains("c.slots.ron"), "{}", err.message);
        assert_eq!(std::fs::read_to_string(&a).expect("read"), "A");
        assert_eq!(std::fs::read_to_string(&b).expect("read"), "B");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o711, "rollback keeps the original file permissions");
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// ADR-10066: `commit_all` and `write_atomic` wrote their temps with `std::fs::write`, which
    /// followed a symlink planted at the predictable `.{name}.{pid}.tmp` name out of the project.
    #[cfg(unix)]
    #[test]
    fn commit_all_and_write_atomic_never_write_through_a_planted_temp_link() {
        let base = regia_test_utils::unique_temp_dir("agent-montage-temp-link");
        let root = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        let a = root.join("a.slots.ron");
        let b = root.join("b.slots.ron");
        std::fs::write(&a, "A").expect("write");
        for (target, victim) in [(&a, "va.ron"), (&b, "vb.ron")] {
            std::os::unix::fs::symlink(outside.join(victim), temp_beside(target)).expect("link");
        }
        commit_all(&[(a.clone(), "A2"), (b.clone(), "B2")]).expect("commits");
        assert!(!outside.join("va.ron").exists() && !outside.join("vb.ron").exists());
        assert_eq!(std::fs::read_to_string(&a).expect("a"), "A2");
        assert_eq!(std::fs::read_to_string(&b).expect("b"), "B2");

        let c = root.join("c.slots.ron");
        std::os::unix::fs::symlink(outside.join("vc.ron"), temp_beside(&c)).expect("link c");
        write_atomic(&c, "C").expect("writes");
        assert!(!outside.join("vc.ron").exists());
        assert_eq!(std::fs::read_to_string(&c).expect("c"), "C");
        assert!(!temp_beside(&c).exists(), "the temp is renamed away");
        std::fs::remove_dir_all(base).ok();
    }

    /// ADR-10066 (review nit): a dangling symlink at the slot table itself is refused like the
    /// other ADR-8798 writers (book/agents/safety.md), not replaced on trust.
    #[cfg(unix)]
    #[test]
    fn slot_edit_refuses_a_dangling_symlink_at_the_table() {
        let base = regia_test_utils::unique_temp_dir("agent-montage-dangling");
        let root = base.join("project");
        std::fs::create_dir_all(&root).expect("root");
        let target = base.join("outside/hero.slots.ron");
        std::os::unix::fs::symlink(&target, root.join("hero.slots.ron")).expect("link");
        let mut world = world_at(&root);
        let err = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "add", "name": "upper" } }),
        )
        .expect_err("dangling link refused");
        assert_eq!(code(&err), "bad_path", "{err:?}");
        assert!(!target.exists());
        assert!(
            std::fs::symlink_metadata(root.join("hero.slots.ron"))
                .expect("link kept")
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn slot_edit_rejects_windows_reserved_names_on_every_host() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-slots-device-names");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        let mut world = world_at(&root);
        // Legal on Linux/Android as a literal file name, reserved on Windows: every host
        // must refuse the same request (regia_core::project_path::check_relative rules).
        for bad in ["C:hero.slots.ron", "anim/NUL.slots.ron", r"anim\CON.slots.ron"] {
            let err = montage_slot_edit(
                &mut world,
                &json!({ "path": bad, "op": { "op": "add", "name": "full" } }),
            )
            .expect_err(bad);
            assert_eq!(code(&err), "bad_path", "{bad}");
        }
        assert!(!root.join("anim/CON.slots.ron").exists());
        std::fs::remove_dir_all(root).ok();
    }

    /// corr-agent (ADR-5597): `anim\hero.slots.ron` passed the shared path rule (which splits on
    /// `\`) but the raw text was joined, so Linux wrote one file named `anim\hero.slots.ron` in
    /// the project root while Windows wrote `anim/hero.slots.ron`.
    #[test]
    fn a_backslash_path_names_the_same_file_on_every_host() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-slots-backslash");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        let mut world = world_at(&root);
        montage_slot_edit(
            &mut world,
            &json!({ "path": r"anim\hero.slots.ron", "op": { "op": "add", "name": "full" } }),
        )
        .expect("add");
        assert!(root.join("anim").join("hero.slots.ron").is_file());
        // Where `\` is not a separator, no stray file named `anim\hero.slots.ron` is written. On
        // Windows that spelling is the file above (ADR-7566).
        if !cfg!(windows) {
            assert!(!root.join(r"anim\hero.slots.ron").exists());
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn slot_table_reaches_graph_slot_nodes() {
        const GRAPH: &str = r#"(format_version: 3, parameters: [], root: Slot((name: "upper",
            source: Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None)))))"#;
        const STRAY: &str = r#"(format_version: 3, parameters: [], root: Slot((name: "tail",
            source: Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None)))))"#;
        let root = regia_test_utils::unique_temp_dir("agent-montage-graph-slots");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        std::fs::write(root.join("anim/hero.animgraph.ron"), GRAPH).expect("write");
        std::fs::write(root.join("anim/stray.animgraph.ron"), STRAY).expect("write");
        let mut world = world_at(&root);
        montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "add", "name": "upper" } }),
        )
        .expect("add");
        let read = montage_slots(&mut world, &json!({ "path": "hero.slots.ron" })).expect("slots");
        assert_eq!(
            read["used_by_graphs"]["upper"],
            json!(["anim/hero.animgraph.ron"])
        );
        assert_eq!(
            read["graph_issues"],
            json!([{ "code": "animgraph_unknown_slot", "graph": "anim/stray.animgraph.ron", "slot": "tail" }])
        );
        // A graph node keeps a slot in use, like a montage track.
        let err = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "delete", "name": "upper" } }),
        )
        .expect_err("in use");
        assert_eq!(code(&err), "slot_in_use");
        assert!(err.message.contains("anim/hero.animgraph.ron"), "{}", err.message);
        // A rename follows into the graph.
        let renamed = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "rename", "from": "upper", "to": "torso" } }),
        )
        .expect("rename");
        assert_eq!(renamed["rewritten"], json!(["anim/hero.animgraph.ron"]));
        let text = std::fs::read_to_string(root.join("anim/hero.animgraph.ron")).expect("read");
        assert_eq!(
            AnimGraph::from_ron_str(&text).expect("valid").slot_names(),
            ["torso"]
        );
        // The stray graph is untouched and `graphs` limits the scan.
        let read = montage_slots(
            &mut world,
            &json!({ "path": "hero.slots.ron", "graphs": ["anim/hero.animgraph.ron"] }),
        )
        .expect("slots");
        assert_eq!(read["graph_issues"], json!([]));
        assert_eq!(
            std::fs::read_to_string(root.join("anim/stray.animgraph.ron")).expect("read"),
            STRAY
        );
        // No temp file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(root.join("anim"))
            .expect("dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_slot_rename_reports_files_it_could_not_parse_and_drops_stale_undo() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-rename-stale");
        std::fs::create_dir_all(root.join("anim")).expect("dir");
        std::fs::write(root.join("anim/combo.montage.ron"), COMBO).expect("write");
        // Names the slot, but is not a montage any more.
        let broken = r#"(format_version: 2, name: "bad", tracks: [(slot: "full", segments: ["#;
        std::fs::write(root.join("anim/broken.montage.ron"), broken).expect("write");
        let graph = r#"(format_version: 3, root: Slot((name: "full", source: "#;
        std::fs::write(root.join("anim/broken.animgraph.ron"), graph).expect("write");
        let mut world = world_at(&root);
        montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "add", "name": "full" } }),
        )
        .expect("add");
        montage_edit(
            &mut world,
            &json!({ "asset": "anim/combo.montage.ron",
                     "edit": { "op": "add_notify", "name": "hit", "time": 0.25 } }),
        )
        .expect("edit");
        let renamed = montage_slot_edit(
            &mut world,
            &json!({ "path": "hero.slots.ron", "op": { "op": "rename", "from": "full", "to": "body" } }),
        )
        .expect("rename");
        assert_eq!(renamed["rewritten"], json!(["anim/combo.montage.ron"]));
        assert_eq!(
            renamed["unparsed"],
            json!(["anim/broken.montage.ron", "anim/broken.animgraph.ron"])
        );
        assert_eq!(
            std::fs::read_to_string(root.join("anim/broken.montage.ron")).expect("read"),
            broken
        );
        // The step before the rename would restore the old slot name, so it is gone.
        let err = montage_edit(
            &mut world,
            &json!({ "asset": "anim/combo.montage.ron", "edit": { "op": "undo" } }),
        )
        .expect_err("no undo over a rename");
        assert_eq!(code(&err), "montage_bad_edit");
        let on_disk = std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read");
        assert_eq!(
            Montage::from_ron_str(&on_disk).expect("valid").tracks[0].slot,
            "body"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_failed_rename_puts_back_the_files_already_replaced() {
        let root = regia_test_utils::unique_temp_dir("agent-montage-commit-all");
        let (file, new_file) = (root.join("a.montage.ron"), root.join("new.slots.ron"));
        std::fs::write(&file, "old").expect("write");
        // A rename onto a directory fails, after the two files before it were replaced.
        let blocked = root.join("blocked.montage.ron");
        std::fs::create_dir_all(blocked.join("child")).expect("dir");
        let err = commit_all(&[
            (file.clone(), "new"),
            (new_file.clone(), "new"),
            (blocked.clone(), "new"),
        ])
        .expect_err("the rename onto a directory fails");
        assert!(err.message.contains("blocked.montage.ron"), "{}", err.message);
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "old");
        assert!(!new_file.exists(), "a file that was new goes away again");
        assert!(blocked.join("child").is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .expect("dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(root).ok();
    }
}
