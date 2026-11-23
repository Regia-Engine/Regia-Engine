// SPDX-License-Identifier: MPL-2.0
//! Open kit (`book/user-guide/open-kit.md`): a synthetic Tincal kit folder written by the test
//! (a skinned GLB with a tagged skeleton root, two clips, an authored LOD1 and `socket_cockpit`;
//! a cockpit GLB; `rig.json`, `sockets.json` and `SHA256SUMS`) opens through the loader, the
//! Kit panel and `editor.open_kit`; hostile folders (bad JSON, a missing model, a hash mismatch,
//! path traversal, symlinks out of the folder, 2 GB size claims) are refused or warned about and
//! never panic or read outside the folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_ecs::prelude::*;
use regia_editor::kit::{
    ChecksumStatus, ChecksumVerdict, ClipSource, KitError, MAX_GLB_BYTES, Sha256, hex, open_kit,
};
use regia_editor::kit_view::{KitView, kit_panel, methods, open_kit_in_world};
use serde_json::{Value, json};

const KIT_ID: &str = "jmc_hauler_v3";

fn temp_root(name: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-open-kit-{}-{name}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn glb_bytes(json: &Value, bin: &[u8]) -> Vec<u8> {
    let mut text = serde_json::to_vec(json).expect("json");
    while !text.len().is_multiple_of(4) {
        text.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + text.len() + if bin.is_empty() { 0 } else { 8 + bin.len() };
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&text);
    if !bin.is_empty() {
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
    }
    out
}

/// Accessors and views over one BIN buffer.
#[derive(Default)]
struct Bin {
    bytes: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Bin {
    fn push(&mut self, data: Vec<u8>, mut accessor: Value) -> usize {
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        self.views
            .push(json!({"buffer": 0, "byteOffset": self.bytes.len(), "byteLength": data.len()}));
        self.bytes.extend(data);
        accessor["bufferView"] = json!(self.views.len() - 1);
        self.accessors.push(accessor);
        self.accessors.len() - 1
    }
}

/// ```text
/// KitRoot ─ body_LOD0 (skinned, 2 tris) ─ Armature (skeleton root, tincal_kit: 1) ─ spine ─ head
///         ├ socket_cockpit (0, 1.5, 0.5)            body_LOD1 (1 tri, authored LOD1)
/// ```
/// Clips: `idle` (spine, 1 s) and `wave` (head, 2 s).
fn model_glb(with_cockpit_socket: bool) -> Vec<u8> {
    let mut bin = Bin::default();
    let positions = [
        [-0.5f32, 0.0, 0.0],
        [0.5, 0.0, 0.0],
        [-0.5, 1.0, 0.0],
        [0.5, 1.0, 0.0],
    ];
    let pos = bin.push(
        f32s(&positions.concat()),
        json!({"componentType": 5126, "count": 4, "type": "VEC3",
               "min": [-0.5, 0.0, 0.0], "max": [0.5, 1.0, 0.0]}),
    );
    let joints = bin.push(
        vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 1, 2, 0, 0],
        json!({"componentType": 5121, "count": 4, "type": "VEC4"}),
    );
    let weights = bin.push(
        f32s(&[
            1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.5, 0.5, 0.0, 0.0, 0.5, 0.5, 0.0, 0.0,
        ]),
        json!({"componentType": 5126, "count": 4, "type": "VEC4"}),
    );
    let indices = bin.push(
        [0u16, 1, 2, 2, 1, 3]
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect(),
        json!({"componentType": 5123, "count": 6, "type": "SCALAR"}),
    );
    let lod1_pos = bin.push(
        f32s(&[-0.5, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 1.0, 0.0]),
        json!({"componentType": 5126, "count": 3, "type": "VEC3",
               "min": [-0.5, 0.0, 0.0], "max": [0.5, 1.0, 0.0]}),
    );
    let identity: Vec<f32> = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
    .repeat(3);
    let ibm = bin.push(
        f32s(&identity),
        json!({"componentType": 5126, "count": 3, "type": "MAT4"}),
    );
    let t_idle = bin.push(
        f32s(&[0.0, 1.0]),
        json!({"componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0]}),
    );
    let t_wave = bin.push(
        f32s(&[0.0, 2.0]),
        json!({"componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [2.0]}),
    );
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let rot = bin.push(
        f32s(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, half, half]),
        json!({"componentType": 5126, "count": 2, "type": "VEC4"}),
    );
    let mut root_children = vec![1, 2, 6];
    if with_cockpit_socket {
        root_children.push(5);
    }
    let document = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [
            {"name": "KitRoot", "children": root_children},
            {"name": "body_LOD0", "mesh": 0, "skin": 0},
            {"name": "Armature", "children": [3], "extras": {"tincal_kit": 1}},
            {"name": "spine", "translation": [0.0, 0.5, 0.0], "children": [4]},
            {"name": "head", "translation": [0.0, 0.4, 0.0]},
            {"name": if with_cockpit_socket { "socket_cockpit" } else { "socket_unused" },
             "translation": [0.0, 1.5, 0.5]},
            {"name": "body_LOD1", "mesh": 1}
        ],
        "skins": [{"joints": [2, 3, 4], "inverseBindMatrices": ibm, "skeleton": 2}],
        "meshes": [
            {"name": "body", "primitives": [{"attributes": {"POSITION": pos, "JOINTS_0": joints,
                "WEIGHTS_0": weights}, "indices": indices}]},
            {"name": "body_low", "primitives": [{"attributes": {"POSITION": lod1_pos}}]}
        ],
        "animations": [
            {"name": "idle", "samplers": [{"input": t_idle, "output": rot}],
             "channels": [{"sampler": 0, "target": {"node": 3, "path": "rotation"}}]},
            {"name": "wave", "samplers": [{"input": t_wave, "output": rot}],
             "channels": [{"sampler": 0, "target": {"node": 4, "path": "rotation"}}]}
        ],
        "accessors": bin.accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}]
    });
    glb_bytes(&document, &bin.bytes)
}

/// One triangle named `cockpit`.
fn cockpit_glb() -> Vec<u8> {
    let mut bin = Bin::default();
    let pos = bin.push(
        f32s(&[-0.2, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0, 0.3, 0.0]),
        json!({"componentType": 5126, "count": 3, "type": "VEC3",
               "min": [-0.2, 0.0, 0.0], "max": [0.2, 0.3, 0.0]}),
    );
    let document = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "cockpit", "mesh": 0}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": pos}}]}],
        "accessors": bin.accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}]
    });
    glb_bytes(&document, &bin.bytes)
}

fn manifest(kit_id: &str) -> String {
    json!({
        "kit_id": kit_id,
        "kind": "hull",
        "class": "heavy",
        "faction": "jmc",
        "replaces": "",
        "stats_from": "jmc_hauler",
        "contract": "production-plan 2026-10-18",
        "version": 3
    })
    .to_string()
}

const RIG: &str = r#"{"stems": {"jmc_hauler_v3": {"nodes": ["KitRoot"], "clips": [
    {"name": "idle", "secs": 1.0},
    {"name": "canopy_open", "joint": "canopy", "axis": [1, 0, 0], "from_deg": 0, "to_deg": 80, "secs": 1.5}
]}}}"#;

const SOCKETS: &str = r#"{"format": 1, "stem": "jmc_hauler_v3", "sockets": [
    {"name": "socket_cockpit", "parent": "KitRoot", "position": [0, 1.5, 0.5]},
    {"name": "socket_exhaust", "parent": "KitRoot", "position": [0, 0.5, -1]}
]}"#;

fn write(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).expect("write kit file");
}

fn write_sums(dir: &Path, names: &[&str]) {
    let mut text = String::new();
    for name in names {
        let bytes = std::fs::read(dir.join(name)).expect("read for sums");
        text.push_str(&format!("{}  {name}\n", hex(&Sha256::digest(&bytes))));
    }
    write(dir, "SHA256SUMS", text.as_bytes());
}

/// A complete, valid kit folder under a fresh temp root.
fn full_kit(name: &str) -> PathBuf {
    let dir = temp_root(name).join(KIT_ID);
    std::fs::create_dir_all(&dir).expect("kit dir");
    write(&dir, "kit.json", manifest(KIT_ID).as_bytes());
    write(&dir, &format!("{KIT_ID}.glb"), &model_glb(true));
    write(&dir, "cockpit_jmc_heavy.glb", &cockpit_glb());
    write(&dir, "rig.json", RIG.as_bytes());
    write(&dir, "sockets.json", SOCKETS.as_bytes());
    write_sums(
        &dir,
        &[
            "kit.json",
            &format!("{KIT_ID}.glb"),
            "cockpit_jmc_heavy.glb",
            "rig.json",
            "sockets.json",
        ],
    );
    dir
}

fn cleanup(dir: &Path) {
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

fn open_kit_handler() -> regia_editor::viewport_agent::Handler {
    methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.open_kit")
        .map(|(_, handler)| handler)
        .expect("editor.open_kit is registered")
}

fn warning_text(warnings: &[regia_editor::kit::KitWarning]) -> Vec<String> {
    warnings.iter().map(ToString::to_string).collect()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_valid_kit_opens_with_metadata_lods_clips_sockets_and_verified_checksums() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = full_kit("valid");
    let kit = open_kit(&dir).expect("kit opens");
    assert!(kit.warnings.is_empty(), "{:?}", warning_text(&kit.warnings));
    assert_eq!(kit.manifest.kit_id, KIT_ID);
    assert_eq!(kit.manifest.kind, "hull");
    assert_eq!(kit.manifest.class, "heavy");
    assert_eq!(kit.manifest.faction, "jmc");
    assert_eq!(kit.manifest.stats_from, "jmc_hauler");
    assert_eq!(kit.manifest.contract, "production-plan 2026-10-18");
    assert_eq!(kit.manifest.version, 3);
    assert!(kit.character_kit, "the skeleton root carries tincal_kit: 1");
    assert_eq!(kit.model.summary.lod_levels, vec![0, 1]);

    assert_eq!(kit.checksums.verdict, ChecksumVerdict::Verified);
    assert_eq!(kit.checksums.entries.len(), 5);
    assert!(kit.checksums.unlisted.is_empty());

    let clips: Vec<(&str, f32, ClipSource)> = kit
        .clips
        .iter()
        .map(|c| (c.name.as_str(), c.duration, c.source))
        .collect();
    assert_eq!(
        clips,
        vec![
            ("idle", 1.0, ClipSource::Model),
            ("wave", 2.0, ClipSource::Model),
            ("canopy_open", 1.5, ClipSource::Rig),
        ]
    );
    assert_eq!(kit.clips[0].rig_secs, Some(1.0));

    let cockpit = kit
        .sockets
        .iter()
        .find(|s| s.name == "socket_cockpit")
        .expect("socket_cockpit");
    assert!(cockpit.in_model && cockpit.in_sockets_json);
    assert_eq!(cockpit.position, Some([0.0, 1.5, 0.5]));
    let exhaust = kit
        .sockets
        .iter()
        .find(|s| s.name == "socket_exhaust")
        .expect("socket_exhaust");
    assert!(!exhaust.in_model && exhaust.in_sockets_json);
    assert_eq!(
        kit.cockpit.as_ref().map(|c| c.file.as_str()),
        Some("cockpit_jmc_heavy.glb")
    );
    let mount = kit.cockpit_mount.expect("cockpit attaches to socket_cockpit");
    assert_eq!([mount[12], mount[13], mount[14]], [0.0, 1.5, 0.5]);

    // The editor: meshes per authored LOD, the cockpit at its socket, the clips in the preview.
    let mut world = World::new();
    open_kit_in_world(&mut world, &dir).expect("kit opens in the editor");
    let view = world.resource::<KitView>();
    let meshes = view.meshes.as_ref().expect("meshes loaded");
    let lods: Vec<(usize, bool, usize)> = meshes
        .lods
        .iter()
        .map(|l| (l.level, l.authored, l.triangles))
        .collect();
    assert_eq!(lods, vec![(0, true, 2), (1, true, 1)]);
    assert_eq!(meshes.cockpit.len(), 1);
    assert!((meshes.cockpit_mount.w_axis.y - 1.5).abs() < 1e-6);
    assert!(
        meshes.bounds.1.y >= 1.8 - 1e-4,
        "bounds include the placed cockpit: {:?}",
        meshes.bounds
    );
    assert!(view.preview_rig, "the skinned model is the animation preview rig");
    let editor = world.resource::<regia_editor::anim::AnimEditor>();
    let rig = editor.rig.as_ref().expect("preview rig");
    assert_eq!(rig.clip_names(), vec!["idle".to_string(), "wave".to_string()]);
    cleanup(&dir);
}

#[test]
fn the_agent_method_reports_the_kit_and_plays_a_clip() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = full_kit("agent");
    let mut world = World::new();
    let handler = open_kit_handler();
    let reply = handler(
        &mut world,
        &json!({ "path": dir.display().to_string(), "clip": "wave" }),
    )
    .expect("editor.open_kit");
    assert_eq!(reply["kit_id"], KIT_ID);
    assert_eq!(reply["manifest"]["faction"], "jmc");
    assert_eq!(reply["manifest"]["version"], 3);
    assert_eq!(reply["checksums"]["verdict"], "verified");
    assert_eq!(reply["warnings"], json!([]));
    assert_eq!(reply["character_kit"], true);
    assert_eq!(reply["cockpit"]["attached_to"], "socket_cockpit");
    assert_eq!(reply["preview_rig"], true);
    assert_eq!(reply["clip_played"], true);
    let clips: Vec<&str> = reply["clips"]
        .as_array()
        .expect("clips")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(clips, ["idle", "wave", "canopy_open"]);
    let sockets: Vec<&str> = reply["sockets"]
        .as_array()
        .expect("sockets")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(sockets, ["socket_cockpit", "socket_exhaust"]);
    assert_eq!(reply["lods"].as_array().map(Vec::len), Some(2));
    let editor = world.resource::<regia_editor::anim::AnimEditor>();
    assert_eq!(editor.ui.clip.as_deref(), Some("wave"));
    assert!(editor.ui.clip_playing);

    // A hostile folder is a typed error, not a panic.
    let bad = temp_root("agent-bad").join("x");
    std::fs::create_dir_all(&bad).expect("dir");
    write(&bad, "kit.json", b"{ not json");
    let err = handler(&mut world, &json!({ "path": bad.display().to_string() })).expect_err("bad kit.json");
    assert_eq!(
        err.data.as_ref().map(|d| d["kit_error"].clone()),
        Some(json!("bad_manifest"))
    );
    let err = handler(&mut world, &json!({ "path": "/definitely/not/a/kit" })).expect_err("no folder");
    assert_eq!(
        err.data.as_ref().map(|d| d["kit_error"].clone()),
        Some(json!("not_a_folder"))
    );
    assert!(handler(&mut world, &json!({})).is_err(), "path is required");
    cleanup(&dir);
    cleanup(&bad);
}

#[test]
fn optional_files_may_be_missing_and_a_missing_socket_puts_the_cockpit_at_the_origin() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_root("minimal").join(KIT_ID);
    std::fs::create_dir_all(&dir).expect("dir");
    write(&dir, "kit.json", manifest(KIT_ID).as_bytes());
    write(&dir, &format!("{KIT_ID}.glb"), &model_glb(true));
    let kit = open_kit(&dir).expect("minimal kit");
    assert!(kit.warnings.is_empty(), "{:?}", warning_text(&kit.warnings));
    assert_eq!(kit.checksums.verdict, ChecksumVerdict::Absent);
    assert!(kit.cockpit.is_none());
    cleanup(&dir);

    let dir = temp_root("no-socket").join(KIT_ID);
    std::fs::create_dir_all(&dir).expect("dir");
    write(&dir, "kit.json", manifest(KIT_ID).as_bytes());
    write(&dir, &format!("{KIT_ID}.glb"), &model_glb(false));
    write(&dir, "cockpit_jmc_heavy.glb", &cockpit_glb());
    let kit = open_kit(&dir).expect("kit without socket_cockpit");
    assert!(kit.cockpit.is_some());
    assert!(kit.cockpit_mount.is_none());
    let warnings = warning_text(&kit.warnings);
    assert!(
        warnings.iter().any(|w| w.contains("placed at the origin")),
        "{warnings:?}"
    );
    let mut world = World::new();
    open_kit_in_world(&mut world, &dir).expect("opens in the editor");
    let view = world.resource::<KitView>();
    let meshes = view.meshes.as_ref().expect("meshes");
    assert_eq!(meshes.cockpit_mount, regia_core::math::Mat4::IDENTITY);
    cleanup(&dir);
}

#[test]
fn hostile_folders_are_refused_or_warned_and_never_read_outside() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Bad JSON.
    let dir = temp_root("bad-json").join(KIT_ID);
    std::fs::create_dir_all(&dir).expect("dir");
    write(&dir, "kit.json", b"{\"kit_id\": 3,");
    assert_eq!(
        open_kit(&dir).map(|_| ()).expect_err("bad json").code(),
        "bad_manifest"
    );
    // No manifest at all.
    std::fs::remove_file(dir.join("kit.json")).expect("rm");
    assert!(matches!(open_kit(&dir), Err(KitError::MissingManifest)));
    cleanup(&dir);

    // Missing model.
    let dir = temp_root("no-glb").join(KIT_ID);
    std::fs::create_dir_all(&dir).expect("dir");
    write(&dir, "kit.json", manifest(KIT_ID).as_bytes());
    assert_eq!(
        open_kit(&dir).map(|_| ()).expect_err("no glb").code(),
        "missing_model"
    );
    // A kit_id that walks out of the folder.
    write(&dir, "kit.json", manifest("../outside").as_bytes());
    assert_eq!(
        open_kit(&dir).map(|_| ()).expect_err("traversal id").code(),
        "invalid_kit_id"
    );
    cleanup(&dir);

    // Hash mismatch: a visible warning, the kit still opens.
    let dir = full_kit("mismatch");
    let rig = std::fs::read(dir.join("rig.json")).expect("rig");
    write(&dir, "rig.json", &[rig.as_slice(), b" "].concat());
    let kit = open_kit(&dir).expect("opens despite the mismatch");
    assert_eq!(kit.checksums.verdict, ChecksumVerdict::Failed);
    let entry = kit
        .checksums
        .entries
        .iter()
        .find(|e| e.file == "rig.json")
        .expect("rig.json entry");
    assert!(matches!(entry.status, ChecksumStatus::Mismatch { .. }));
    let warnings = warning_text(&kit.warnings);
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("rig.json: checksum mismatch")),
        "{warnings:?}"
    );

    // Path traversal in a listed file name: refused, even when the outside file's hash matches.
    let outside = dir.parent().expect("parent").join("outside.txt");
    std::fs::write(&outside, b"secret").expect("outside file");
    let digest = hex(&Sha256::digest(b"secret"));
    let mut sums = std::fs::read_to_string(dir.join("SHA256SUMS")).expect("sums");
    sums.push_str(&format!("{digest}  ../outside.txt\n{digest}  /etc/passwd\n"));
    write(&dir, "SHA256SUMS", sums.as_bytes());
    let kit = open_kit(&dir).expect("opens");
    for name in ["../outside.txt", "/etc/passwd"] {
        let entry = kit
            .checksums
            .entries
            .iter()
            .find(|e| e.file == name)
            .expect("listed entry");
        assert!(
            matches!(&entry.status, ChecksumStatus::Refused { reason } if reason.contains("path") || reason.contains("leaves")),
            "{name}: {:?}",
            entry.status
        );
    }
    cleanup(&dir);

    // A header that claims 2 GB.
    let dir = temp_root("claim").join(KIT_ID);
    std::fs::create_dir_all(&dir).expect("dir");
    write(&dir, "kit.json", manifest(KIT_ID).as_bytes());
    let mut glb = model_glb(true);
    glb[8..12].copy_from_slice(&2_000_000_000u32.to_le_bytes());
    write(&dir, &format!("{KIT_ID}.glb"), &glb);
    let err = open_kit(&dir).map(|_| ()).expect_err("2 GB claim");
    assert_eq!(err.code(), "bad_model");
    assert!(err.to_string().contains("claims 2000000000 bytes"), "{err}");
    // A JSON chunk that claims 2 GB.
    let mut glb = model_glb(true);
    glb[12..16].copy_from_slice(&2_000_000_000u32.to_le_bytes());
    write(&dir, &format!("{KIT_ID}.glb"), &glb);
    assert_eq!(
        open_kit(&dir).map(|_| ()).expect_err("JSON claim").code(),
        "bad_model"
    );
    cleanup(&dir);

    // Real 2 GiB files (sparse where the file system allows): the model is refused by size and
    // an oversized sidecar is a warning; neither is read or hashed.
    let dir = full_kit("big");
    let big = std::fs::File::create(dir.join("rig.json")).expect("big rig");
    if big.set_len(2 << 30).is_ok() {
        drop(big);
        let kit = open_kit(&dir).expect("an oversized sidecar is a warning");
        let warnings = warning_text(&kit.warnings);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("rig.json: ignored") && w.contains("limit")),
            "{warnings:?}"
        );
        let entry = kit
            .checksums
            .entries
            .iter()
            .find(|e| e.file == "rig.json")
            .expect("entry");
        assert!(matches!(&entry.status, ChecksumStatus::Refused { reason } if reason.contains("limit")));
        let model = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(format!("{KIT_ID}.glb")))
            .expect("model");
        model.set_len(MAX_GLB_BYTES + 1).expect("grow the model");
        drop(model);
        assert_eq!(
            open_kit(&dir).map(|_| ()).expect_err("huge model").code(),
            "too_large"
        );
    }
    cleanup(&dir);

    // A file that is a folder.
    let dir = full_kit("dir-as-file");
    std::fs::remove_file(dir.join("sockets.json")).expect("rm");
    std::fs::create_dir(dir.join("sockets.json")).expect("dir in place of the file");
    let kit = open_kit(&dir).expect("opens");
    let warnings = warning_text(&kit.warnings);
    assert!(
        warnings.iter().any(|w| w.contains("not a regular file")),
        "{warnings:?}"
    );
    cleanup(&dir);
}

fn sums_status(dir: &Path, listed: &str) -> ChecksumStatus {
    let kit = open_kit(dir).expect("kit opens");
    kit.checksums
        .entries
        .iter()
        .find(|e| e.file == listed)
        .unwrap_or_else(|| panic!("no entry for {listed}: {:?}", kit.checksums.entries))
        .status
        .clone()
}

fn append_sums(dir: &Path, line_name: &str, digest: &str) {
    let mut text = std::fs::read_to_string(dir.join("SHA256SUMS")).expect("sums");
    text.push_str(&format!("{digest}  {line_name}\n"));
    write(dir, "SHA256SUMS", text.as_bytes());
}

#[test]
fn sums_may_list_in_folder_subpaths() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = full_kit("subpath");
    std::fs::create_dir_all(dir.join("textures")).expect("textures");
    write(&dir, "textures/x.png", b"png bytes");
    append_sums(&dir, "textures/x.png", &hex(&Sha256::digest(b"png bytes")));
    let kit = open_kit(&dir).expect("kit opens");
    assert_eq!(sums_status(&dir, "textures/x.png"), ChecksumStatus::Ok);
    assert_eq!(
        kit.checksums.verdict,
        ChecksumVerdict::Verified,
        "{:?}",
        kit.checksums
    );

    // A changed file keeps the mismatch error; a missing one stays missing.
    write(&dir, "textures/x.png", b"tampered");
    assert!(matches!(
        sums_status(&dir, "textures/x.png"),
        ChecksumStatus::Mismatch { .. }
    ));
    std::fs::remove_file(dir.join("textures/x.png")).expect("rm");
    assert_eq!(sums_status(&dir, "textures/x.png"), ChecksumStatus::Missing);
    cleanup(&dir);
}

#[test]
fn sums_refuse_every_hostile_subpath_form() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = full_kit("subpath-hostile");
    std::fs::create_dir_all(dir.join("textures")).expect("textures");
    write(&dir, "textures/x.png", b"png bytes");
    write(dir.parent().expect("parent"), "outside.txt", b"secret");
    let digest = hex(&Sha256::digest(b"png bytes"));
    for bad in [
        "../outside.txt",
        "textures/../../outside.txt",
        "textures/../kit.json",
        "textures/./x.png",
        "/etc/passwd",
        "textures//x.png",
        "textures/",
        "textures\\x.png",
        "C:/x.png",
        "C:x.png",
        "textures/x:y.png",
        "textures/x\0.png",
    ] {
        let dir_bad = full_kit("subpath-hostile-case");
        let mut text = std::fs::read_to_string(dir_bad.join("SHA256SUMS")).expect("sums");
        text.push_str(&format!("{digest}  {bad}\n"));
        write(&dir_bad, "SHA256SUMS", text.as_bytes());
        let kit = open_kit(&dir_bad).expect("kit opens; the entry is refused");
        let status = kit
            .checksums
            .entries
            .iter()
            .find(|e| e.file == bad)
            .map(|e| e.status.clone());
        assert!(
            matches!(status, Some(ChecksumStatus::Refused { .. })),
            "{bad:?}: {status:?}"
        );
        assert_eq!(kit.checksums.verdict, ChecksumVerdict::Failed, "{bad:?}");
        cleanup(&dir_bad);
    }
    cleanup(&dir);
}

#[cfg(unix)]
#[test]
fn sums_refuse_subpath_symlinks_that_leave_the_folder() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use std::os::unix::fs::symlink;

    let dir = full_kit("subpath-symlink");
    let outside = dir.parent().expect("parent").join("outside-dir");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::fs::write(outside.join("y.png"), b"secret").expect("outside file");
    let digest = hex(&Sha256::digest(b"secret"));
    // A linked file, and a linked directory.
    std::fs::create_dir_all(dir.join("textures")).expect("textures");
    symlink(outside.join("y.png"), dir.join("textures/y.png")).expect("file link");
    symlink(&outside, dir.join("linked")).expect("dir link");
    append_sums(&dir, "textures/y.png", &digest);
    append_sums(&dir, "linked/y.png", &digest);
    for listed in ["textures/y.png", "linked/y.png"] {
        assert!(
            matches!(sums_status(&dir, listed), ChecksumStatus::Refused { reason } if reason.contains("leaves")),
            "{listed}"
        );
    }
    // A link that stays inside is fine.
    write(&dir, "textures/z.png", b"inside");
    symlink(dir.join("textures/z.png"), dir.join("textures/z-link.png")).expect("inside link");
    append_sums(&dir, "textures/z-link.png", &hex(&Sha256::digest(b"inside")));
    assert_eq!(sums_status(&dir, "textures/z-link.png"), ChecksumStatus::Ok);
    cleanup(&dir);
}

#[test]
fn the_generic_faction_validates_without_a_warning() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = full_kit("generic");
    let manifest = json!({
        "kit_id": KIT_ID, "kind": "hull", "class": "heavy", "faction": "generic",
        "replaces": "", "stats_from": "jmc_hauler", "contract": "x", "version": 3
    })
    .to_string();
    write(&dir, "kit.json", manifest.as_bytes());
    write(&dir, "cockpit_generic_heavy.glb", &cockpit_glb());
    std::fs::remove_file(dir.join("cockpit_jmc_heavy.glb")).expect("rm");
    write_sums(
        &dir,
        &[
            "kit.json",
            &format!("{KIT_ID}.glb"),
            "cockpit_generic_heavy.glb",
            "rig.json",
            "sockets.json",
        ],
    );
    let kit = open_kit(&dir).expect("kit opens");
    assert!(kit.warnings.is_empty(), "{:?}", warning_text(&kit.warnings));
    assert!(kit.cockpit.is_some());
    cleanup(&dir);
}

#[cfg(unix)]
#[test]
fn symlinks_out_of_the_folder_are_refused() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use std::os::unix::fs::symlink;

    let dir = full_kit("symlink");
    let outside = dir.parent().expect("parent").join("outside-rig.json");
    std::fs::write(&outside, RIG).expect("outside");
    std::fs::remove_file(dir.join("rig.json")).expect("rm");
    symlink(&outside, dir.join("rig.json")).expect("symlink");
    let kit = open_kit(&dir).expect("a symlinked sidecar is a warning");
    assert!(kit.rig_clips.is_empty(), "the outside rig.json was not read");
    let warnings = warning_text(&kit.warnings);
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("rig.json") && w.contains("leaves the kit folder")),
        "{warnings:?}"
    );
    let entry = kit
        .checksums
        .entries
        .iter()
        .find(|e| e.file == "rig.json")
        .expect("entry");
    assert!(matches!(&entry.status, ChecksumStatus::Refused { reason } if reason.contains("leaves")));

    // A symlink that stays inside is fine.
    std::fs::remove_file(dir.join("rig.json")).expect("rm");
    std::fs::write(dir.join("rig-real.json"), RIG).expect("inside");
    symlink(dir.join("rig-real.json"), dir.join("rig.json")).expect("inside symlink");
    let kit = open_kit(&dir).expect("opens");
    assert_eq!(kit.rig_clips.len(), 2);

    // The model itself linked out of the folder: refused.
    let model = dir.join(format!("{KIT_ID}.glb"));
    let outside_model = dir.parent().expect("parent").join("outside.glb");
    std::fs::rename(&model, &outside_model).expect("move model out");
    symlink(&outside_model, &model).expect("model symlink");
    assert_eq!(
        open_kit(&dir).map(|_| ()).expect_err("model link").code(),
        "unsafe_path"
    );

    // A dangling link.
    std::fs::remove_file(&model).expect("rm");
    symlink(dir.join("nowhere.glb"), &model).expect("dangling");
    assert_eq!(
        open_kit(&dir).map(|_| ()).expect_err("dangling").code(),
        "unsafe_path"
    );
    cleanup(&dir);
}

/// Text drawn in one egui frame's output.
fn frame_text(shapes: &[egui::epaint::ClippedShape]) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    for clipped in shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

fn panel_frame(ctx: &egui::Context, world: &mut World) -> String {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| kit_panel(ui, world, None));
    });
    output.textures_delta.clear();
    frame_text(&output.shapes)
}

#[test]
fn the_kit_panel_shows_metadata_checksums_clips_sockets_and_warnings() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = full_kit("panel");
    let mut world = World::new();
    world.insert_resource(KitView::default());
    world.resource_mut::<KitView>().open_request = Some(dir.clone());
    let ctx = egui::Context::default();
    let _ = panel_frame(&ctx, &mut world);
    let text = panel_frame(&ctx, &mut world);
    assert!(
        world.resource::<KitView>().kit.is_some(),
        "the request opened the kit"
    );
    for needle in [
        KIT_ID,
        "production-plan 2026-10-18",
        "jmc_hauler",
        "SHA256SUMS: verified (5 listed)",
        "idle",
        "wave",
        "canopy_open",
        "rig.json only",
        "socket_cockpit",
        "socket_exhaust",
        "Socket gizmos",
        "No GPU device",
    ] {
        assert!(text.contains(needle), "the panel shows `{needle}`:\n{text}");
    }

    // A mismatch shows as FAILED with the warning listed.
    let rig = std::fs::read(dir.join("rig.json")).expect("rig");
    write(&dir, "rig.json", &[rig.as_slice(), b"\n"].concat());
    world.resource_mut::<KitView>().open_request = Some(dir.clone());
    let _ = panel_frame(&ctx, &mut world);
    let text = panel_frame(&ctx, &mut world);
    assert!(text.contains("SHA256SUMS: FAILED"), "{text}");
    assert!(text.contains("Warnings (1)"), "{text}");
    assert!(text.contains("checksum mismatch"), "{text}");

    // A broken kit shows the error and keeps no kit.
    write(&dir, "kit.json", b"[]");
    world.resource_mut::<KitView>().open_request = Some(dir.clone());
    let text = panel_frame(&ctx, &mut world);
    assert!(world.resource::<KitView>().kit.is_none());
    assert!(text.contains("Kit not opened"), "{text}");
    cleanup(&dir);
}

#[test]
fn the_kit_renders_with_the_cockpit_and_socket_marks_on_lavapipe() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let dir = full_kit("render");
    let mut world = World::new();
    world.insert_resource(device);
    let reply = open_kit_handler()(
        &mut world,
        &json!({ "path": dir.display().to_string(), "render": true, "width": 256, "height": 256 }),
    )
    .expect("render");
    let render = &reply["render"];
    assert_eq!(render["mime"], "image/png");
    assert!(render["png_base64"].as_str().is_some_and(|png| png.len() > 100));
    let marks: Vec<&str> = render["socket_marks"]
        .as_array()
        .expect("marks")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(marks.contains(&"socket_cockpit"), "{marks:?}");
    let lod1 = open_kit_handler()(
        &mut world,
        &json!({ "path": dir.display().to_string(), "render": true, "lod": 1 }),
    )
    .expect("LOD1 render");
    assert_eq!(lod1["render"]["lod"], 1);
    assert!(
        open_kit_handler()(
            &mut world,
            &json!({ "path": dir.display().to_string(), "render": true, "lod": 9 })
        )
        .is_err()
    );
    cleanup(&dir);
}

/// The kit's budget meters (`book/agents/editor-budget.md`): LOD0 triangles only, the authored
/// LOD chain, the class from `kit.json` (a tagged skeleton is a character), the meters in
/// `editor.open_kit`'s reply, `editor.budget { source: "kit" }` with a class and limit
/// overrides, and the Budget line in the panel.
#[test]
fn the_kit_budget_measures_lod0_and_the_lod_chain() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_editor::budget::BudgetStatus;
    use regia_editor::model_report::ModelClass;

    let dir = full_kit("budget");
    let mut world = World::new();
    open_kit_in_world(&mut world, &dir).expect("kit opens");
    let view = world.resource::<KitView>();
    assert_eq!(
        view.budget_class(),
        ModelClass::Character,
        "the tagged skeleton picks the class"
    );
    let budget = view.budget().expect("the model was measured");
    assert_eq!(
        budget.row("triangles").map(|row| row.value),
        Some(2),
        "LOD1 is not counted with LOD0"
    );
    let lods: Vec<(usize, u64, bool)> = budget
        .lods
        .iter()
        .map(|lod| (lod.level, lod.triangles, lod.reduces))
        .collect();
    assert_eq!(lods, [(0, 2, true), (1, 1, true)]);
    assert_eq!(budget.worst(), BudgetStatus::Ok);

    let reply = open_kit_handler()(&mut world, &json!({ "path": dir.display().to_string() })).expect("open");
    assert_eq!(reply["budget"]["class"], "character");
    assert_eq!(reply["budget"]["status"], "ok");
    assert_eq!(reply["budget"]["rows"][0]["key"], "triangles");
    assert_eq!(reply["budget"]["rows"][0]["value"], 2);
    assert_eq!(reply["budget"]["lods"][1]["ratio"], 0.5);

    let budget_method = regia_editor::budget::methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.budget")
        .map(|(_, handler)| handler)
        .expect("editor.budget is registered");
    let tight = budget_method(
        &mut world,
        &json!({ "source": "kit", "class": "prop", "limits": { "max_triangles": 1 } }),
    )
    .expect("editor.budget");
    assert_eq!(tight["budget"]["class"], "prop");
    assert_eq!(tight["budget"]["status"], "over");
    assert_eq!(tight["budget"]["over"], 1);

    let ctx = egui::Context::default();
    let _ = panel_frame(&ctx, &mut world);
    let text = panel_frame(&ctx, &mut world);
    assert!(
        text.contains("Budget character · 2/"),
        "the panel shows the budget line:\n{text}"
    );
    cleanup(&dir);
}
