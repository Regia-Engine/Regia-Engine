// SPDX-License-Identifier: MPL-2.0
//! `regia dcc-feed`: the reference client of the DCC scene feed (`docs/specs/60-dcc-scene-feed.md`
//! § 5.1, `book/agents/dcc-feed.md`).
//!
//! `send` streams a glTF file as a DCC add-on would: one item per node, a skeleton per skin, the
//! full state on each `resync`. `--watch` re-reads the file and sends what differs; `--animate`
//! plays a clip as frames. `record` and `replay` wrap the agent methods; `dump` prints a recording.
//! The scene-to-message logic is pure ([`load_scene`], [`FeedScene::full_state`], [`diff_scenes`]),
//! so tests read the messages without a socket.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_agent::client::Client;
use regia_agent::dcc_feed::recording::Recording;
use regia_agent::dcc_feed::wire::{
    self, CameraFrame, ClipPush, ClipTrack, Elem, FeedKind, FrameInfo, FrameReader, Hello, ItemDeclare,
    JointStatic, Lens, LightFrame, Limits, MeshSection, MeshUpdate, Message, PROTOCOL_VERSION, PoseFrame,
    Reassembler, SkeletonStatic, TexturePush, TextureSource, TransformBlock, TransformFrame, section,
};
use serde_json::json;

/// Largest payload of one frame; bigger messages travel as fragments.
const MAX_FRAGMENT: usize = 16 << 20;
/// How long `send` waits for the server to apply what it sent before saying goodbye.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often `--watch` looks at the file's modification time.
const WATCH_POLL: Duration = Duration::from_millis(200);

/// `regia dcc-feed` subcommands.
#[derive(Debug, Subcommand)]
pub enum DccFeedCommand {
    /// Stream a glTF file into a running editor or dev game.
    Send(SendArgs),
    /// Start recording a session to a `.rfdrec` (`--stop` ends it).
    Record(RecordArgs),
    /// Replay a `.rfdrec` in a running app.
    Replay(ReplayArgs),
    /// Print one line per record of a `.rfdrec`.
    Dump(DumpArgs),
}

/// `regia dcc-feed send`.
#[derive(Debug, Args)]
pub struct SendArgs {
    /// The `.gltf` or `.glb` file.
    pub file: PathBuf,
    /// `tcp:<ip>:<port>`, `unix:<path>` or `<ip>:<port>` (default: the project's `endpoint.json`).
    #[arg(long)]
    pub endpoint: Option<String>,
    /// The session token file (default: the project's `.regia/dcc_feed/token`).
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Project directory (default: the current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Keep running and send what changes when the file changes.
    #[arg(long)]
    pub watch: bool,
    /// Play this glTF animation as frames.
    #[arg(long)]
    pub animate: Option<String>,
    /// Frames per second of `--animate`.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,
    /// Also write what was sent to this `.rfdrec`.
    #[arg(long)]
    pub record: Option<PathBuf>,
    /// The client name the server's allow list sees.
    #[arg(long, default_value = "regia-dcc-feed")]
    pub client_name: String,
}

/// `regia dcc-feed record`.
#[derive(Debug, Args)]
pub struct RecordArgs {
    /// The `.rfdrec`, relative to the project (required unless `--stop`).
    pub out: Option<String>,
    /// Stop the running recording and write it.
    #[arg(long)]
    pub stop: bool,
    /// Project directory (default: the current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
}

/// `regia dcc-feed replay`.
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// The `.rfdrec`, relative to the project.
    pub input: String,
    /// Fixed ticks between records (default: the recorded spacing).
    #[arg(long)]
    pub ticks_per_record: Option<u32>,
    /// Restart at the end.
    #[arg(long = "loop")]
    pub looping: bool,
    /// Client name of the replay session.
    #[arg(long)]
    pub as_client: Option<String>,
    /// Project directory (default: the current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
}

/// `regia dcc-feed dump`.
#[derive(Debug, Args)]
pub struct DumpArgs {
    /// The `.rfdrec`.
    pub input: PathBuf,
}

/// Runs a `regia dcc-feed` subcommand.
///
/// # Errors
///
/// A failed connection, an unreadable file, or a server refusal.
pub fn run(command: &DccFeedCommand) -> Result<()> {
    match command {
        DccFeedCommand::Send(args) => send(args),
        DccFeedCommand::Record(args) => {
            let params = if args.stop {
                json!({ "action": "stop" })
            } else {
                let Some(out) = args.out.as_ref() else {
                    bail!("`record` needs the output path, or --stop");
                };
                json!({ "action": "start", "path": out })
            };
            agent_call(&args.project, "dcc_feed.record", params)
        }
        DccFeedCommand::Replay(args) => {
            let mut params = json!({ "path": args.input, "loop": args.looping });
            if let Some(n) = args.ticks_per_record {
                params["ticks_per_record"] = json!(n);
            }
            if let Some(name) = &args.as_client {
                params["as_client"] = json!(name);
            }
            agent_call(&args.project, "dcc_feed.replay", params)
        }
        DccFeedCommand::Dump(args) => {
            let recording = Recording::read_file(&args.input)
                .map_err(|e| anyhow::anyhow!("{}: {e}", args.input.display()))?;
            for line in recording.dump_lines(&Limits::default()) {
                println!("{line}");
            }
            Ok(())
        }
    }
}

fn agent_call(project: &Path, method: &str, params: serde_json::Value) -> Result<()> {
    let project = project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", project.display()))?;
    let mut client =
        Client::connect_project(&project).context("cannot connect to a running Regia Engine app")?;
    let result = client.call(method, params).context("method call failed")?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The scene as messages
// ---------------------------------------------------------------------------------------------

/// What an item carries.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemData {
    /// Only a placement.
    Plain,
    /// A lens.
    Camera(Lens),
    /// An emitter: color, intensity, range, inner and outer cone angles.
    Light {
        /// Linear RGB.
        color: [f32; 3],
        /// glTF units.
        intensity: f32,
        /// 0 = none.
        range: f32,
        /// Inner cone angle.
        inner: f32,
        /// Outer cone angle.
        outer: f32,
    },
    /// Geometry sections, in wire order.
    Mesh(Vec<MeshSection>),
    /// Joints (bind pose with skipped nodes folded in) and where each comes from in the file.
    Skeleton(Vec<JointStatic>, Vec<JointSource>),
}

/// Where a skeleton joint comes from in the glTF node tree.
///
/// A joint's wire parent is its nearest ancestor that is a joint of the same skin; the nodes in
/// between (or, for a root joint, every ancestor up to the scene root) are not joints, so their
/// transforms are folded into the joint's local transform: `chain[0] * … * chain[n-1] * local`.
#[derive(Debug, Clone, PartialEq)]
pub struct JointSource {
    /// The joint's glTF node.
    pub node: usize,
    /// The node's own rest transform.
    pub local: TransformBlock,
    /// The skipped nodes and their rest transforms, outermost first (empty when the parent node is
    /// the parent joint).
    pub chain: Vec<(usize, TransformBlock)>,
}

/// One item of a loaded scene.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneItem {
    /// The declaration.
    pub declare: ItemDeclare,
    /// Parent-local placement.
    pub xf: TransformBlock,
    /// Kind data.
    pub data: ItemData,
    /// The glTF node (`usize::MAX` for a skeleton).
    pub node: usize,
}

/// One animation channel, sampled by [`Track::value`].
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// The node it drives.
    pub node: usize,
    /// 0 translation, 1 rotation, 2 scale.
    pub property: u8,
    /// Key times, increasing.
    pub times: Vec<f32>,
    /// Flattened values, 3 or 4 per key.
    pub values: Vec<f32>,
    /// True for STEP interpolation.
    pub step: bool,
}

/// A glTF animation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SampledClip {
    /// The animation's name.
    pub name: String,
    /// Length in seconds.
    pub duration_s: f32,
    /// Its tracks.
    pub tracks: Vec<Track>,
}

/// A glTF file as feed items.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FeedScene {
    /// Items, parents first.
    pub items: Vec<SceneItem>,
    /// Texture pushes.
    pub textures: Vec<TexturePush>,
    /// Animations.
    pub clips: Vec<SampledClip>,
}

/// Frame ids per handle: strictly increasing, across resyncs and watch updates.
#[derive(Debug, Default)]
pub struct FrameIds {
    next: HashMap<u32, u64>,
    /// `source_time_ns` of the frames being built.
    pub time_ns: u64,
}

impl FrameIds {
    fn info(&mut self, handle: u32) -> FrameInfo {
        let id = self.next.entry(handle).or_insert(0);
        *id += 1;
        FrameInfo {
            frame_id: *id,
            source_time_ns: self.time_ns,
            timecode: None,
        }
    }
}

fn floats<const N: usize>(values: &[[f32; N]]) -> Vec<u8> {
    values.iter().flatten().flat_map(|v| v.to_le_bytes()).collect()
}

fn section_of(section_id: u8, elem: Elem, index: u16, count: usize, data: Vec<u8>) -> MeshSection {
    MeshSection {
        section_id,
        elem,
        index,
        count: u32::try_from(count).unwrap_or(u32::MAX),
        data,
    }
}

/// No translation, no rotation, unit scale.
const IDENTITY: ([f32; 3], [f32; 4], [f32; 3]) = ([0.0; 3], [0.0, 0.0, 0.0, 1.0], [1.0; 3]);

fn block(t: ([f32; 3], [f32; 4], [f32; 3])) -> TransformBlock {
    TransformBlock {
        translation: t.0,
        rotation: t.1,
        scale: t.2,
        world_space: false,
    }
}

/// Hamilton product `a * b` of two `[x, y, z, w]` quaternions.
fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// `v` rotated by the unit quaternion `q`.
fn quat_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = q;
    // t = 2 * (q.xyz × v); v' = v + w * t + q.xyz × t
    let t = [
        2.0 * (y * v[2] - z * v[1]),
        2.0 * (z * v[0] - x * v[2]),
        2.0 * (x * v[1] - y * v[0]),
    ];
    [
        v[0] + w * t[0] + (y * t[2] - z * t[1]),
        v[1] + w * t[1] + (z * t[0] - x * t[2]),
        v[2] + w * t[2] + (x * t[1] - y * t[0]),
    ]
}

/// The parent-then-child transform `parent * child` as translation, rotation and scale (glTF
/// node math, `M = T * R * S`).
///
/// A uniform parent scale commutes with rotation, so that path multiplies the components and keeps
/// a uniform input bit for bit. A non-uniform parent scale does not: the scale is the column
/// lengths of `R_parent * S_parent * R_child * S_child` and the rotation is that matrix with the
/// lengths removed. A product that shears (the columns are not orthogonal) cannot be stored as
/// TRS; the rotation is then only the nearest orthonormal reading of those axes.
fn compose(parent: &TransformBlock, child: &TransformBlock) -> TransformBlock {
    if parent.scale[0] == parent.scale[1] && parent.scale[1] == parent.scale[2] {
        return compose_uniform_parent(parent, child);
    }
    let parent_axes = trs_columns(parent.rotation, parent.scale);
    let child_axes = trs_columns(child.rotation, child.scale);
    let linear = [
        mul_columns(parent_axes, child_axes[0]),
        mul_columns(parent_axes, child_axes[1]),
        mul_columns(parent_axes, child_axes[2]),
    ];
    let moved = mul_columns(parent_axes, child.translation);
    let translation = [
        parent.translation[0] + moved[0],
        parent.translation[1] + moved[1],
        parent.translation[2] + moved[2],
    ];
    if !translation.iter().all(|n| n.is_finite()) || linear.iter().flatten().any(|n| !n.is_finite()) {
        return compose_uniform_parent(parent, child);
    }
    let (rotation, scale) = decompose_linear(linear);
    if !rotation.iter().all(|n| n.is_finite()) || !scale.iter().all(|n| n.is_finite()) {
        return compose_uniform_parent(parent, child);
    }
    TransformBlock {
        translation,
        rotation,
        scale,
        world_space: false,
    }
}

/// Component-wise TRS product. Exact when `parent`'s scale is uniform (including a uniform
/// reflection) or `child` does not rotate.
fn compose_uniform_parent(parent: &TransformBlock, child: &TransformBlock) -> TransformBlock {
    let scaled = [
        parent.scale[0] * child.translation[0],
        parent.scale[1] * child.translation[1],
        parent.scale[2] * child.translation[2],
    ];
    let moved = quat_rotate(parent.rotation, scaled);
    let mut rotation = quat_mul(parent.rotation, child.rotation);
    let len = rotation.iter().map(|x| x * x).sum::<f32>().sqrt();
    if len > 0.0 {
        rotation.iter_mut().for_each(|x| *x /= len);
    }
    TransformBlock {
        translation: [
            parent.translation[0] + moved[0],
            parent.translation[1] + moved[1],
            parent.translation[2] + moved[2],
        ],
        rotation,
        scale: [
            parent.scale[0] * child.scale[0],
            parent.scale[1] * child.scale[1],
            parent.scale[2] * child.scale[2],
        ],
        world_space: false,
    }
}

/// Columns of `R * diag(scale)`.
fn trs_columns(rotation: [f32; 4], scale: [f32; 3]) -> [[f32; 3]; 3] {
    [
        quat_rotate(rotation, [scale[0], 0.0, 0.0]),
        quat_rotate(rotation, [0.0, scale[1], 0.0]),
        quat_rotate(rotation, [0.0, 0.0, scale[2]]),
    ]
}

fn mul_columns(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Scale is the column length (X negated when the linear map reflects). The rotation is the
/// matrix left after dividing those lengths out.
fn decompose_linear(mut cols: [[f32; 3]; 3]) -> ([f32; 4], [f32; 3]) {
    let mut scale = [
        dot3(cols[0], cols[0]).sqrt(),
        dot3(cols[1], cols[1]).sqrt(),
        dot3(cols[2], cols[2]).sqrt(),
    ];
    if dot3(cols[0], cross3(cols[1], cols[2])) < 0.0 {
        scale[0] = -scale[0];
    }
    let fallback = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for (i, col) in cols.iter_mut().enumerate() {
        let len = scale[i];
        if len.is_finite() && len.abs() > 1.0e-20 {
            *col = [col[0] / len, col[1] / len, col[2] / len];
        } else {
            *col = fallback[i];
            scale[i] = 0.0;
        }
    }
    (quat_from_basis(cols), scale)
}

/// Unit quaternion of an orthonormal basis, columns as the rotated axes. A degenerate basis
/// becomes the identity.
fn quat_from_basis(cols: [[f32; 3]; 3]) -> [f32; 4] {
    let (m00, m10, m20) = (cols[0][0], cols[0][1], cols[0][2]);
    let (m01, m11, m21) = (cols[1][0], cols[1][1], cols[1][2]);
    let (m02, m12, m22) = (cols[2][0], cols[2][1], cols[2][2]);
    let trace = m00 + m11 + m22;
    let q = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [(m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [(m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s]
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [(m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s]
    };
    let len = q.iter().map(|x| x * x).sum::<f32>().sqrt();
    if len.is_finite() && len > 0.0 {
        q.map(|x| x / len)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// `chain[0] * … * chain[n-1] * local`; `local` unchanged (bit for bit) for an empty chain.
fn fold_chain<'a>(
    chain: impl DoubleEndedIterator<Item = &'a TransformBlock>,
    local: TransformBlock,
) -> TransformBlock {
    chain.rev().fold(local, |acc, parent| compose(parent, &acc))
}

fn wire_str(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    let n = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&bytes[..usize::from(n)]);
}

fn image_key(image: &gltf::Image<'_>) -> String {
    image
        .name()
        .map_or_else(|| format!("image_{}", image.index()), str::to_owned)
}

/// Every primitive of `mesh` merged into one vertex list, one submesh per primitive.
/// `joint_remap` maps a skin slot (the glTF `JOINTS_0` value) to the joint's index in the bound
/// skeleton item's list, which [`skeleton_item`] may have reordered parents first.
fn mesh_sections(
    mesh: &gltf::Mesh<'_>,
    buffers: &[gltf::buffer::Data],
    joint_remap: Option<&[u16]>,
) -> Vec<MeshSection> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut submeshes: Vec<u8> = Vec::new();
    let mut slots: Vec<u8> = Vec::new();
    let mut slot_count = 0usize;
    let mut has_normals = true;
    let mut has_uvs = true;
    let mut joints: Vec<[u16; 4]> = Vec::new();
    let mut weights: Vec<[f32; 4]> = Vec::new();
    let mut has_skin = true;
    for primitive in mesh.primitives() {
        if primitive.mode() != gltf::mesh::Mode::Triangles {
            continue;
        }
        let reader = primitive.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
        let Some(read) = reader.read_positions() else {
            continue;
        };
        let base = u32::try_from(positions.len()).unwrap_or(u32::MAX);
        let start = positions.len();
        positions.extend(read);
        let count = positions.len() - start;
        match reader.read_normals() {
            Some(n) => normals.extend(n),
            None => has_normals = false,
        }
        match reader.read_tex_coords(0) {
            Some(t) => uvs.extend(t.into_f32()),
            None => has_uvs = false,
        }
        match (reader.read_joints(0), reader.read_weights(0)) {
            (Some(j), Some(w)) => {
                let remap = |slot: u16| {
                    joint_remap
                        .and_then(|m| m.get(usize::from(slot)).copied())
                        .unwrap_or(slot)
                };
                joints.extend(j.into_u16().map(|v| v.map(remap)));
                weights.extend(w.into_f32());
            }
            _ => has_skin = false,
        }
        let first = u32::try_from(indices.len()).unwrap_or(u32::MAX);
        match reader.read_indices() {
            Some(read) => indices.extend(read.into_u32().map(|i| i.saturating_add(base))),
            None => indices.extend(base..base + u32::try_from(count).unwrap_or(u32::MAX)),
        }
        let n = u32::try_from(indices.len()).unwrap_or(u32::MAX) - first;
        let slot = u32::try_from(slot_count).unwrap_or(u32::MAX);
        submeshes.extend(slot.to_le_bytes());
        submeshes.extend(first.to_le_bytes());
        submeshes.extend(n.to_le_bytes());
        write_slot(&mut slots, &primitive.material());
        slot_count += 1;
    }
    let vertices = positions.len();
    let mut out = vec![section_of(
        section::POSITIONS,
        Elem::F32,
        0,
        vertices,
        floats(&positions),
    )];
    if has_skin && joints.len() == vertices && weights.len() == vertices {
        let bytes: Vec<u8> = joints.iter().flatten().flat_map(|j| j.to_le_bytes()).collect();
        out.push(section_of(section::JOINTS, Elem::U16, 0, vertices, bytes));
        out.push(section_of(
            section::WEIGHTS,
            Elem::F32,
            0,
            vertices,
            floats(&weights),
        ));
    }
    if has_normals && normals.len() == vertices {
        out.push(section_of(
            section::NORMALS,
            Elem::F32,
            0,
            vertices,
            floats(&normals),
        ));
    }
    if has_uvs && uvs.len() == vertices {
        out.push(section_of(section::UV, Elem::F32, 0, vertices, floats(&uvs)));
    }
    let bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    out.push(section_of(section::INDICES, Elem::U32, 0, indices.len(), bytes));
    out.push(section_of(
        section::SUBMESHES,
        Elem::U32,
        0,
        slot_count,
        submeshes,
    ));
    out.push(section_of(
        section::MATERIAL_SLOTS,
        Elem::U8,
        0,
        slot_count,
        slots,
    ));
    out
}

/// One row of the slot table (spec § 4.5 id 8).
fn write_slot(out: &mut Vec<u8>, material: &gltf::Material<'_>) {
    let pbr = material.pbr_metallic_roughness();
    wire_str(out, material.name().unwrap_or("default"));
    for v in pbr.base_color_factor() {
        out.extend(v.to_le_bytes());
    }
    out.extend(pbr.metallic_factor().to_le_bytes());
    out.extend(pbr.roughness_factor().to_le_bytes());
    for v in material.emissive_factor() {
        out.extend(v.to_le_bytes());
    }
    let name = |t: Option<gltf::Texture<'_>>| t.map(|t| image_key(&t.source())).unwrap_or_default();
    wire_str(out, &name(pbr.base_color_texture().map(|t| t.texture())));
    wire_str(out, &name(material.normal_texture().map(|t| t.texture())));
    wire_str(out, &name(pbr.metallic_roughness_texture().map(|t| t.texture())));
    wire_str(out, &name(material.emissive_texture().map(|t| t.texture())));
}

fn lens_of(camera: &gltf::Camera<'_>) -> Lens {
    match camera.projection() {
        gltf::camera::Projection::Perspective(p) => Lens::Perspective {
            yfov_rad: p.yfov(),
            aspect: p.aspect_ratio().unwrap_or(0.0),
            near: p.znear(),
            far: p.zfar().unwrap_or(0.0),
        },
        gltf::camera::Projection::Orthographic(o) => Lens::Orthographic {
            xmag: o.xmag(),
            ymag: o.ymag(),
            near: o.znear(),
            far: o.zfar(),
        },
    }
}

fn light_of(light: &gltf::khr_lights_punctual::Light<'_>) -> (u8, ItemData) {
    use gltf::khr_lights_punctual::Kind;
    let (variant, inner, outer) = match light.kind() {
        Kind::Directional => (0, 0.0, 0.0),
        Kind::Point => (1, 0.0, 0.0),
        Kind::Spot {
            inner_cone_angle,
            outer_cone_angle,
        } => (2, inner_cone_angle, outer_cone_angle),
    };
    (
        variant,
        ItemData::Light {
            color: light.color(),
            intensity: light.intensity(),
            range: light.range().unwrap_or(0.0),
            inner,
            outer,
        },
    )
}

/// Reads the textures the file embeds or references, as encoded bytes.
fn texture_pushes(
    path: &Path,
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
) -> Vec<TexturePush> {
    let mut out = Vec::new();
    let spaces = image_colorspaces(document);
    for image in document.images() {
        let (bytes, mime) = match image.source() {
            gltf::image::Source::View { view, mime_type } => {
                let Some(data) = buffers.get(view.buffer().index()) else {
                    continue;
                };
                let Some(slice) = view
                    .offset()
                    .checked_add(view.length())
                    .and_then(|end| data.0.get(view.offset()..end))
                else {
                    continue;
                };
                (slice.to_vec(), mime_type.to_owned())
            }
            gltf::image::Source::Uri { uri, mime_type } => {
                if let Some(rest) = uri.strip_prefix("data:") {
                    let bytes = match regia_asset_pipeline::data_uri_bytes(uri) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            eprintln!("regia dcc-feed: cannot decode embedded texture: {error}");
                            continue;
                        }
                    };
                    let declared = rest.split(';').next().filter(|mime| !mime.is_empty());
                    (bytes, mime_type.or(declared).unwrap_or("image/jpeg").to_owned())
                } else {
                    let Some(decoded) = regia_asset_pipeline::decode_gltf_uri(uri) else {
                        eprintln!("regia dcc-feed: texture URI is not UTF-8");
                        continue;
                    };
                    let file = path.parent().unwrap_or_else(|| Path::new(".")).join(&decoded);
                    let Ok(bytes) = std::fs::read(&file) else {
                        eprintln!("regia dcc-feed: cannot read texture {}", file.display());
                        continue;
                    };
                    (
                        bytes,
                        mime_type.unwrap_or_else(|| mime_of_uri(&decoded)).to_owned(),
                    )
                }
            }
        };
        out.push(TexturePush {
            name: image_key(&image),
            colorspace: spaces.get(&image.index()).copied().unwrap_or(0),
            source: TextureSource::Encoded {
                container: container_of(&mime),
                bytes,
            },
        });
    }
    out
}

/// The MIME type a texture file extension names (unknown extensions stay JPEG, the old default).
fn mime_of_uri(uri: &str) -> &'static str {
    let lower = uri.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".exr") {
        "image/x-exr"
    } else if lower.ends_with(".ktx2") {
        "image/ktx2"
    } else {
        "image/jpeg"
    }
}

/// The `texture_push` container byte of a MIME type (spec 60 § 4.4: 0 PNG, 1 JPEG, 2 EXR, 3 KTX2).
fn container_of(mime: &str) -> u8 {
    match mime {
        "image/png" => 0,
        "image/x-exr" | "image/aces" => 2,
        "image/ktx2" => 3,
        _ => 1,
    }
}

/// The `texture_push` colorspace per image index: 2 for a normal map, 1 for data maps
/// (metal-rough, occlusion), 0 (sRGB) for color maps and anything unreferenced.
fn image_colorspaces(document: &gltf::Document) -> HashMap<usize, u8> {
    let mut out: HashMap<usize, u8> = HashMap::new();
    let mut mark = |texture: Option<gltf::Texture<'_>>, space: u8| {
        if let Some(t) = texture {
            let slot = out.entry(t.source().index()).or_insert(space);
            // Normal beats data beats color if one image is shared.
            *slot = (*slot).max(space);
        }
    };
    for material in document.materials() {
        mark(material.normal_texture().map(|t| t.texture()), 2);
        let pbr = material.pbr_metallic_roughness();
        mark(pbr.metallic_roughness_texture().map(|t| t.texture()), 1);
        mark(material.occlusion_texture().map(|t| t.texture()), 1);
        mark(pbr.base_color_texture().map(|t| t.texture()), 0);
        mark(material.emissive_texture().map(|t| t.texture()), 0);
    }
    out
}

/// A cubic-spline sampler stores in-tangent, value and out-tangent per key. The feed plays keys
/// as linear, so keep the middle value of each triple. `None` when the length is not a whole
/// number of `3 * width` triples for `keys` keys.
fn strip_cubic_tangents(values: &[f32], width: usize, keys: usize) -> Option<Vec<f32>> {
    if values.len() != keys * width * 3 {
        return None;
    }
    let mut out = Vec::with_capacity(keys * width);
    for key in 0..keys {
        let base = key * width * 3 + width;
        out.extend_from_slice(&values[base..base + width]);
    }
    Some(out)
}

fn clips_of(document: &gltf::Document, buffers: &[gltf::buffer::Data]) -> Vec<SampledClip> {
    let mut out = Vec::new();
    for animation in document.animations() {
        let mut clip = SampledClip {
            name: animation
                .name()
                .map_or_else(|| format!("animation_{}", animation.index()), str::to_owned),
            ..SampledClip::default()
        };
        for channel in animation.channels() {
            let reader = channel.reader(|b| buffers.get(b.index()).map(|d| d.0.as_slice()));
            let (Some(times), Some(outputs)) = (reader.read_inputs(), reader.read_outputs()) else {
                continue;
            };
            let times: Vec<f32> = times.collect();
            let (property, values): (u8, Vec<f32>) = match outputs {
                gltf::animation::util::ReadOutputs::Translations(v) => (0, v.flatten().collect()),
                gltf::animation::util::ReadOutputs::Rotations(v) => (1, v.into_f32().flatten().collect()),
                gltf::animation::util::ReadOutputs::Scales(v) => (2, v.flatten().collect()),
                gltf::animation::util::ReadOutputs::MorphTargetWeights(_) => continue,
            };
            let width = if property == 1 { 4 } else { 3 };
            let cubic = channel.sampler().interpolation() == gltf::animation::Interpolation::CubicSpline;
            let values = if cubic {
                match strip_cubic_tangents(&values, width, times.len()) {
                    Some(v) => v,
                    None => continue,
                }
            } else if values.len() == times.len() * width {
                values
            } else {
                // A track whose value count disagrees with its key count would desync the wire.
                continue;
            };
            clip.duration_s = clip.duration_s.max(times.last().copied().unwrap_or(0.0));
            clip.tracks.push(Track {
                node: channel.target().node().index(),
                property,
                times,
                values,
                step: channel.sampler().interpolation() == gltf::animation::Interpolation::Step,
            });
        }
        out.push(clip);
    }
    out
}

/// The node tree as the skins see it: each node's parent and rest transform.
struct NodeTree {
    parent: HashMap<usize, usize>,
    rest: Vec<TransformBlock>,
}

impl NodeTree {
    fn of(document: &gltf::Document) -> Self {
        let mut parent = HashMap::new();
        for node in document.nodes() {
            for child in node.children() {
                // A node with two parents is invalid glTF; the first one listed wins.
                parent.entry(child.index()).or_insert(node.index());
            }
        }
        let rest = document
            .nodes()
            .map(|n| block(n.transform().decomposed()))
            .collect();
        Self { parent, rest }
    }

    /// The nearest ancestor of `node` in `joints` and the non-joint nodes passed on the way,
    /// outermost first. A cycle (invalid glTF) stops when a node repeats, so the lap is folded once.
    fn joint_parent(&self, node: usize, joints: &HashMap<usize, usize>) -> (Option<usize>, Vec<usize>) {
        let mut skipped = Vec::new();
        let mut seen = HashSet::new();
        seen.insert(node);
        let mut at = node;
        while let Some(&up) = self.parent.get(&at) {
            if !seen.insert(up) {
                break;
            }
            if joints.contains_key(&up) {
                skipped.reverse();
                return (Some(up), skipped);
            }
            skipped.push(up);
            at = up;
        }
        skipped.reverse();
        (None, skipped)
    }
}

/// The skeleton item of a skin, and the wire index of each file slot of the skin's `joints` list.
/// Skeletons come first, so a skinned mesh can name its parent.
///
/// A joint's parent is its nearest ancestor in the skin's joint list; the nodes between them (all
/// ancestors, for a root joint) are folded into its local transform ([`JointSource`]). Joints are
/// listed parents first, as `skeleton_static` requires: a list that already is keeps its order.
///
/// Also returns the skin slot to wire index map that the skinned meshes' joint indices go through.
fn skeleton_item(skin: &gltf::Skin<'_>, position: usize, tree: &NodeTree) -> (SceneItem, Vec<u16>) {
    let handle = u32::try_from(position + 1).unwrap_or(u32::MAX);
    let listed: Vec<gltf::Node<'_>> = skin.joints().collect();
    let slot_of: HashMap<usize, usize> = listed.iter().enumerate().map(|(i, n)| (n.index(), i)).collect();
    let links: Vec<(Option<usize>, Vec<usize>)> = listed
        .iter()
        .map(|j| tree.joint_parent(j.index(), &slot_of))
        .collect();
    // Parents first: emit each joint after its parent joint, keeping the file order otherwise.
    let mut order: Vec<usize> = Vec::with_capacity(listed.len());
    let mut placed = vec![false; listed.len()];
    for start in 0..listed.len() {
        let mut pending = Vec::new();
        let mut at = Some(start);
        while let Some(i) = at.filter(|i| !placed[*i] && !pending.contains(i)) {
            pending.push(i);
            at = links[i].0.and_then(|p| slot_of.get(&p).copied());
        }
        for i in pending.into_iter().rev() {
            placed[i] = true;
            order.push(i);
        }
    }
    let wire_index: HashMap<usize, i32> = order
        .iter()
        .enumerate()
        .map(|(w, slot)| (listed[*slot].index(), i32::try_from(w).unwrap_or(-1)))
        .collect();
    let mut remap: Vec<u16> = (0..listed.len())
        .map(|slot| u16::try_from(slot).unwrap_or(u16::MAX))
        .collect();
    for (wire, slot) in order.iter().enumerate() {
        remap[*slot] = u16::try_from(wire).unwrap_or(u16::MAX);
    }
    let rest = |n: usize| tree.rest.get(n).copied().unwrap_or_else(|| block(IDENTITY));
    let mut list: Vec<JointStatic> = Vec::with_capacity(order.len());
    let mut sources: Vec<JointSource> = Vec::with_capacity(order.len());
    for slot in order {
        let j = &listed[slot];
        let (parent, skipped) = &links[slot];
        let source = JointSource {
            node: j.index(),
            local: rest(j.index()),
            chain: skipped.iter().map(|n| (*n, rest(*n))).collect(),
        };
        let bind = fold_chain(source.chain.iter().map(|(_, xf)| xf), source.local);
        list.push(JointStatic {
            name: j
                .name()
                .map_or_else(|| format!("joint_{}", j.index()), str::to_owned),
            parent: parent.and_then(|p| wire_index.get(&p).copied()).unwrap_or(-1),
            t: bind.translation,
            r: bind.rotation,
            s: bind.scale,
        });
        sources.push(source);
    }
    let key = skin
        .name()
        .map_or_else(|| format!("skin_{}", skin.index()), str::to_owned);
    let item = SceneItem {
        declare: ItemDeclare {
            handle,
            display_name: key.clone(),
            key,
            kind: FeedKind::Skeleton,
            parent_handle: 0,
            variant: 0,
            channels: Vec::new(),
        },
        xf: block(IDENTITY),
        data: ItemData::Skeleton(list, sources),
        node: usize::MAX,
    };
    (item, remap)
}

/// The joints that are items as well as skeleton joints: a joint that owns a mesh, camera or
/// light, or that has a non-joint node below it (whose placement follows the joint). Every joint
/// between such a node and the nearest non-joint ancestor is one too, so the item chain matches the
/// node chain.
fn attached_joints(
    document: &gltf::Document,
    tree: &NodeTree,
    joints: &std::collections::BTreeSet<usize>,
) -> std::collections::BTreeSet<usize> {
    let mut out = std::collections::BTreeSet::new();
    for node in document.nodes() {
        let index = node.index();
        let owns = node.mesh().is_some() || node.camera().is_some() || node.light().is_some();
        // A skinned mesh is placed by its skeleton, not by the nodes above it.
        let skinned = node.skin().is_some();
        if (joints.contains(&index) && !owns) || (skinned && !joints.contains(&index)) {
            continue;
        }
        if joints.contains(&index) {
            out.insert(index);
        }
        // Mark the joints above this node, up to the scene root (a cycle stops after one lap).
        let mut at = index;
        for _ in 0..tree.rest.len() {
            let Some(&up) = tree.parent.get(&at) else {
                break;
            };
            if up == index {
                break;
            }
            if joints.contains(&up) {
                out.insert(up);
            }
            at = up;
        }
    }
    out
}

/// Loads a glTF file as feed items.
///
/// # Errors
///
/// The file does not parse.
pub fn load_scene(path: &Path) -> Result<FeedScene> {
    let (document, buffers) = regia_asset_pipeline::open_gltf_checked(path)
        .map_err(|why| anyhow::anyhow!(why))
        .with_context(|| format!("cannot read {}", path.display()))?;
    let document = &document;
    let scene = document
        .default_scene()
        .or_else(|| document.scenes().next())
        .context("the file has no scene")?;

    let tree = NodeTree::of(document);
    let mut joint_nodes = std::collections::BTreeSet::new();
    for skin in document.skins() {
        joint_nodes.extend(skin.joints().map(|j| j.index()));
    }
    let attached = attached_joints(document, &tree, &joint_nodes);
    let (mut items, joint_remaps): (Vec<SceneItem>, Vec<Vec<u16>>) = document
        .skins()
        .enumerate()
        .map(|(i, skin)| skeleton_item(&skin, i, &tree))
        .unzip();
    // Skeleton keys are identities too: make them unique and reserve them before naming nodes.
    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    for item in &mut items {
        let base = item.declare.key.clone();
        let seen = names.entry(base.clone()).or_insert(0);
        *seen += 1;
        if *seen > 1 {
            item.declare.key = format!("{base}#{seen}");
            item.declare.display_name.clone_from(&item.declare.key);
        }
    }
    // A skinned node's item sits under its skeleton item, which is how a mesh finds its bones.
    let skin_handles: Vec<u32> = items.iter().map(|i| i.declare.handle).collect();
    let mut handles: HashMap<usize, u32> = HashMap::new();
    let mut stack: Vec<(gltf::Node<'_>, u32)> = scene.nodes().map(|n| (n, 0)).collect();
    stack.reverse();
    // A node reached twice (two parents or a cycle: invalid glTF) is emitted once.
    let mut visited = std::collections::BTreeSet::new();
    while let Some((node, parent)) = stack.pop() {
        if !visited.insert(node.index()) {
            continue;
        }
        // A joint is a pose of its skeleton; it is an item too only when something hangs on it.
        let pose_only = joint_nodes.contains(&node.index()) && !attached.contains(&node.index());
        let mut handle = parent;
        if !pose_only {
            handle = u32::try_from(items.len() + 1).unwrap_or(u32::MAX);
            handles.insert(node.index(), handle);
            let base = node
                .name()
                .map_or_else(|| format!("node_{}", node.index()), str::to_owned);
            let seen = names.entry(base.clone()).or_insert(0);
            *seen += 1;
            let key = if *seen == 1 {
                base
            } else {
                format!("{base}#{seen}")
            };
            let (kind, variant, data) = if let Some(mesh) = node.mesh() {
                {
                    let remap = node
                        .skin()
                        .and_then(|skin| joint_remaps.get(skin.index()))
                        .map(Vec::as_slice);
                    (
                        FeedKind::Mesh,
                        0,
                        ItemData::Mesh(mesh_sections(&mesh, &buffers, remap)),
                    )
                }
            } else if let Some(camera) = node.camera() {
                let lens = lens_of(&camera);
                let variant = u8::from(matches!(lens, Lens::Orthographic { .. }));
                (FeedKind::Camera, variant, ItemData::Camera(lens))
            } else if let Some(light) = node.light() {
                let (variant, data) = light_of(&light);
                (FeedKind::Light, variant, data)
            } else {
                (FeedKind::Transform, 0, ItemData::Plain)
            };
            let skeleton = node
                .skin()
                .and_then(|skin| skin_handles.get(skin.index()).copied());
            items.push(SceneItem {
                declare: ItemDeclare {
                    handle,
                    display_name: key.clone(),
                    key,
                    kind,
                    parent_handle: skeleton.unwrap_or(parent),
                    variant,
                    channels: Vec::new(),
                },
                xf: block(node.transform().decomposed()),
                data,
                node: node.index(),
            });
        }
        let first_child = stack.len();
        stack.extend(node.children().map(|c| (c, handle)));
        stack[first_child..].reverse();
    }
    Ok(FeedScene {
        items,
        textures: texture_pushes(path, document, &buffers),
        clips: clips_of(document, &buffers),
    })
}

fn pose_of(joints: &[JointStatic], handle: u32, frames: &mut FrameIds) -> Message {
    let values: Vec<f32> = joints
        .iter()
        .flat_map(|j| j.t.into_iter().chain(j.r).chain(j.s))
        .collect();
    Message::Pose(PoseFrame {
        handle,
        info: frames.info(handle),
        layout: 0,
        count: u16::try_from(joints.len()).unwrap_or(u16::MAX),
        values,
    })
}

/// The frame that carries an item's placement.
fn placement_frame(item: &SceneItem, frames: &mut FrameIds) -> Message {
    let handle = item.declare.handle;
    match &item.data {
        ItemData::Camera(lens) => Message::Camera(CameraFrame {
            handle,
            info: frames.info(handle),
            xf: item.xf,
            lens: *lens,
            physical: None,
        }),
        ItemData::Light {
            color,
            intensity,
            range,
            inner,
            outer,
        } => Message::Light(LightFrame {
            handle,
            info: frames.info(handle),
            xf: item.xf,
            color: *color,
            intensity: *intensity,
            range: *range,
            spot_inner_rad: *inner,
            spot_outer_rad: *outer,
        }),
        _ => Message::Transform(TransformFrame {
            handle,
            info: frames.info(handle),
            xf: item.xf,
        }),
    }
}

fn mesh_frame(handle: u32, update_mode: u8, sections: Vec<MeshSection>, frames: &mut FrameIds) -> Message {
    Message::Mesh(MeshUpdate {
        handle,
        info: frames.info(handle),
        update_mode,
        sections,
    })
}

impl FeedScene {
    /// Declarations, static data and one frame per item: what a `resync` asks for.
    #[must_use]
    pub fn full_state(&self, frames: &mut FrameIds) -> Vec<Message> {
        let mut out: Vec<Message> = self.textures.iter().cloned().map(Message::TexturePush).collect();
        for item in &self.items {
            out.push(Message::ItemDeclare(item.declare.clone()));
            let handle = item.declare.handle;
            match &item.data {
                ItemData::Skeleton(joints, _) => {
                    out.push(Message::SkeletonStatic(SkeletonStatic {
                        handle,
                        joints: joints.clone(),
                    }));
                    out.push(pose_of(joints, handle, frames));
                }
                ItemData::Mesh(sections) => {
                    out.push(placement_frame(item, frames));
                    out.push(mesh_frame(handle, 0, sections.clone(), frames));
                }
                _ => out.push(placement_frame(item, frames)),
            }
        }
        out.extend(
            self.clips
                .iter()
                .map(|clip| Message::ClipPush(self.clip_push(clip))),
        );
        out
    }

    /// A clip as `clip_push`: tracks name their item or joint, and it binds to the item the first
    /// track drives.
    ///
    /// A joint with skipped nodes above it ([`JointSource::chain`]) has the chain in its bind, so
    /// its keys get it too ([`folded_track`]), as `--animate` does per frame: a baked clip replaces
    /// the joint's rest value with them. When a skipped node is animated, the joint gets a track
    /// even if it has none of its own.
    fn clip_push(&self, clip: &SampledClip) -> ClipPush {
        let mut tracks = Vec::new();
        let mut bind_handle = 0;
        // (joint node, property) pairs already sent as folded tracks.
        let mut folded: std::collections::BTreeSet<(usize, u8)> = std::collections::BTreeSet::new();
        for track in &clip.tracks {
            let found = self.items.iter().find_map(|item| match &item.data {
                ItemData::Skeleton(joints, sources) => joints
                    .iter()
                    .zip(sources)
                    .find(|(_, source)| source.node == track.node)
                    .map(|(j, source)| (j.name.clone(), item.declare.handle, Some(source))),
                _ => (item.node == track.node).then(|| (item.declare.key.clone(), item.declare.handle, None)),
            });
            let Some((target, handle, source)) = found else {
                continue;
            };
            let pushed = match source.filter(|s| !s.chain.is_empty()) {
                Some(source) => {
                    if !folded.insert((source.node, track.property)) {
                        continue;
                    }
                    folded_track(clip, source, track.property, Some(track), target)
                }
                None => Some(ClipTrack {
                    target,
                    property: track.property,
                    interp: u8::from(!track.step),
                    times: track.times.clone(),
                    values: track.values.clone(),
                }),
            };
            let Some(pushed) = pushed else {
                continue;
            };
            if bind_handle == 0 {
                bind_handle = handle;
            }
            tracks.push(pushed);
        }
        // Joints that only move because a skipped node above them does.
        for item in &self.items {
            let ItemData::Skeleton(joints, sources) = &item.data else {
                continue;
            };
            for (joint, source) in joints.iter().zip(sources) {
                if source.chain.is_empty() {
                    continue;
                }
                for property in 0..3u8 {
                    if folded.contains(&(source.node, property)) {
                        continue;
                    }
                    let Some(pushed) = folded_track(clip, source, property, None, joint.name.clone()) else {
                        continue;
                    };
                    folded.insert((source.node, property));
                    if bind_handle == 0 {
                        bind_handle = item.declare.handle;
                    }
                    tracks.push(pushed);
                }
            }
        }
        ClipPush {
            name: clip.name.clone(),
            fps: 0.0,
            duration_s: clip.duration_s,
            looping: false,
            bind_handle,
            tracks,
        }
    }
}

/// `rest` with every track of `clip` that drives `node` applied at `t` (only `property`'s, when
/// given).
fn posed_at(
    clip: &SampledClip,
    node: usize,
    rest: &TransformBlock,
    t: f32,
    property: Option<u8>,
) -> TransformBlock {
    let mut xf = *rest;
    let tracks = clip
        .tracks
        .iter()
        .filter(|k| k.node == node && property.is_none_or(|p| p == k.property));
    for track in tracks {
        match (track.property, track.value(t).as_slice()) {
            (0, [x, y, z]) => xf.translation = [*x, *y, *z],
            (1, [x, y, z, w]) => xf.rotation = [*x, *y, *z, *w],
            (2, [x, y, z]) => xf.scale = [*x, *y, *z],
            _ => {}
        }
    }
    xf
}

/// One property (0 translation, 1 rotation, 2 scale) of a transform as wire floats.
fn property_of(xf: &TransformBlock, property: u8) -> Vec<f32> {
    match property {
        0 => xf.translation.to_vec(),
        1 => xf.rotation.to_vec(),
        _ => xf.scale.to_vec(),
    }
}

/// The track of one property of a folded joint (`source.chain` not empty), keyed as
/// `chain * local`, the same fold as its bind.
///
/// Each property of `parent * child` depends on the child's same property only, so with a static
/// chain each of `own`'s keys maps on its own (translation `chain.t + chain.r·(chain.s ⊙ t)`,
/// rotation `chain.r·r`, scale `chain.s ⊙ s`), and linear or step playback between the mapped keys
/// matches the unmapped clip exactly. When a chain node is animated (a translation depends on
/// every property of the chain; a rotation or scale on the same property), the joint is resampled
/// at the union of the key times, with the same posing as [`SampledClip::frames_at`]: exact at the
/// keys, linear between them. `None` when nothing drives the property.
fn folded_track(
    clip: &SampledClip,
    source: &JointSource,
    property: u8,
    own: Option<&Track>,
    target: String,
) -> Option<ClipTrack> {
    let in_chain = |node: usize| source.chain.iter().any(|(n, _)| *n == node);
    let moving: Vec<&Track> = clip
        .tracks
        .iter()
        .filter(|k| in_chain(k.node) && (property == 0 || k.property == property))
        .collect();
    let width = if property == 1 { 4 } else { 3 };
    let fold = |chain: &[TransformBlock], local: TransformBlock| fold_chain(chain.iter(), local);
    let with_key = |values: &[f32]| {
        let mut local = source.local;
        match (property, values) {
            (0, [x, y, z]) => local.translation = [*x, *y, *z],
            (1, [x, y, z, w]) => local.rotation = [*x, *y, *z, *w],
            (2, [x, y, z]) => local.scale = [*x, *y, *z],
            _ => {}
        }
        local
    };
    if moving.is_empty() {
        let own = own?;
        let chain: Vec<TransformBlock> = source.chain.iter().map(|(_, xf)| *xf).collect();
        let values = own
            .values
            .chunks(width)
            .flat_map(|key| property_of(&fold(&chain, with_key(key)), property))
            .collect();
        return Some(ClipTrack {
            target,
            property,
            interp: u8::from(!own.step),
            times: own.times.clone(),
            values,
        });
    }
    let mut times: Vec<f32> = moving
        .iter()
        .copied()
        .chain(own)
        .flat_map(|k| k.times.iter().copied())
        .filter(|t| t.is_finite())
        .collect();
    times.sort_by(f32::total_cmp);
    times.dedup();
    let step = moving.iter().copied().chain(own).all(|k| k.step);
    let mut values: Vec<f32> = Vec::with_capacity(times.len() * width);
    for t in &times {
        let local = posed_at(clip, source.node, &source.local, *t, Some(property));
        let chain: Vec<TransformBlock> = source
            .chain
            .iter()
            .map(|(node, rest)| posed_at(clip, *node, rest, *t, None))
            .collect();
        let mut key = property_of(&fold(&chain, local), property);
        // Keep neighboring rotation keys in one hemisphere, so playback takes the short way.
        if width == 4 && values.len() >= 4 {
            let previous = &values[values.len() - 4..];
            if previous.iter().zip(&key).map(|(a, b)| a * b).sum::<f32>() < 0.0 {
                key.iter_mut().for_each(|x| *x = -*x);
            }
        }
        values.extend(key);
    }
    Some(ClipTrack {
        target,
        property,
        interp: u8::from(!step),
        times,
        values,
    })
}

/// True for sections a vertex-only update may carry.
fn per_vertex(id: u8) -> bool {
    matches!(
        id,
        section::POSITIONS
            | section::NORMALS
            | section::TANGENTS
            | section::UV
            | section::COLOR
            | section::WEIGHTS
    )
}

/// `new` with the handles the session already uses: a key keeps its old handle and a new key takes
/// the next free one. Handles belong to the session, not the file: an item added early in a file
/// shifts every later position, and the server still knows the old numbers.
#[must_use]
pub fn adopt_handles(old: &FeedScene, new: &FeedScene) -> FeedScene {
    let known: HashMap<&str, u32> = old
        .items
        .iter()
        .map(|i| (i.declare.key.as_str(), i.declare.handle))
        .collect();
    let mut last = old.items.iter().map(|i| i.declare.handle).max().unwrap_or(0);
    let mut handles: HashMap<u32, u32> = HashMap::new();
    for item in &new.items {
        let handle = known.get(item.declare.key.as_str()).copied().unwrap_or_else(|| {
            last += 1;
            last
        });
        handles.insert(item.declare.handle, handle);
    }
    let mut out = new.clone();
    for item in &mut out.items {
        let map = |handle: u32| handles.get(&handle).copied().unwrap_or(handle);
        item.declare.handle = map(item.declare.handle);
        item.declare.parent_handle = map(item.declare.parent_handle);
    }
    out
}

/// The messages that turn the `old` file into the `new` one: one `transform` for a moved node, a
/// vertex-only or full `mesh` for edited geometry, and nothing for what did not change. Matching
/// is by key, and handles stay the old file's (spec § 5.1).
#[must_use]
pub fn diff_scenes(old: &FeedScene, new: &FeedScene, frames: &mut FrameIds) -> Vec<Message> {
    let new = adopt_handles(old, new);
    let mut out: Vec<Message> = new
        .textures
        .iter()
        .filter(|t| !old.textures.contains(t))
        .cloned()
        .map(Message::TexturePush)
        .collect();
    let by_key: HashMap<&str, &SceneItem> = old.items.iter().map(|i| (i.declare.key.as_str(), i)).collect();
    for item in &new.items {
        let previous = by_key.get(item.declare.key.as_str());
        // A key cannot change kind in place (`kind_mismatch`, spec § 2.4 rule 5).
        let changed_kind = previous.is_some_and(|p| p.declare.kind != item.declare.kind);
        let Some(previous) = previous.filter(|_| !changed_kind) else {
            if changed_kind {
                out.push(Message::ItemDelete(item.declare.handle));
            }
            let single = FeedScene {
                items: vec![item.clone()],
                ..FeedScene::default()
            };
            out.extend(single.full_state(frames));
            continue;
        };
        if item.declare != previous.declare {
            out.push(Message::ItemDeclare(item.declare.clone()));
        }
        diff_item(item, previous, frames, &mut out);
    }
    let kept: std::collections::BTreeSet<&str> = new.items.iter().map(|i| i.declare.key.as_str()).collect();
    for item in &old.items {
        if !kept.contains(item.declare.key.as_str()) {
            out.push(Message::ItemDelete(item.declare.handle));
        }
    }
    out.extend(
        new.clips
            .iter()
            .filter(|clip| !old.clips.contains(clip))
            .map(|clip| Message::ClipPush(new.clip_push(clip))),
    );
    out
}

/// What one item that stays needs: its placement and kind data, each only when different.
fn diff_item(item: &SceneItem, previous: &SceneItem, frames: &mut FrameIds, out: &mut Vec<Message>) {
    let handle = item.declare.handle;
    match (&item.data, &previous.data) {
        (ItemData::Mesh(new_sections), ItemData::Mesh(old_sections)) => {
            if item.xf != previous.xf {
                out.push(placement_frame(item, frames));
            }
            if new_sections == old_sections {
                return;
            }
            let same_topology = new_sections.len() == old_sections.len()
                && new_sections.iter().zip(old_sections).all(|(a, b)| {
                    a.section_id == b.section_id && a.count == b.count && (per_vertex(a.section_id) || a == b)
                });
            if same_topology {
                let changed: Vec<MeshSection> = new_sections
                    .iter()
                    .zip(old_sections)
                    .filter(|(a, b)| a != b)
                    .map(|(a, _)| a.clone())
                    .collect();
                out.push(mesh_frame(handle, 1, changed, frames));
            } else {
                out.push(mesh_frame(handle, 0, new_sections.clone(), frames));
            }
        }
        (ItemData::Skeleton(joints, _), ItemData::Skeleton(old_joints, _)) => {
            if joints != old_joints {
                out.push(Message::SkeletonStatic(SkeletonStatic {
                    handle,
                    joints: joints.clone(),
                }));
                out.push(pose_of(joints, handle, frames));
            }
        }
        (data, old_data) => {
            if item.xf != previous.xf || data != old_data {
                out.push(placement_frame(item, frames));
            }
        }
    }
}

impl Track {
    /// Value at `t`: linear between keys (rotations normalized), held before the first key and
    /// after the last.
    #[must_use]
    pub fn value(&self, t: f32) -> Vec<f32> {
        let width = if self.property == 1 { 4 } else { 3 };
        let key = |i: usize| &self.values[i * width..(i + 1) * width];
        let last = self.times.len().saturating_sub(1);
        if self.times.is_empty() || self.values.len() < self.times.len() * width {
            return Vec::new();
        }
        if t <= self.times[0] {
            return key(0).to_vec();
        }
        if t >= self.times[last] {
            return key(last).to_vec();
        }
        let hi = self.times.iter().position(|k| *k >= t).unwrap_or(last);
        let lo = hi.saturating_sub(1);
        if self.step {
            // STEP holds the greatest key at or before t, including an exact interior key.
            return key(if self.times[hi] == t { hi } else { lo }).to_vec();
        }
        let span = self.times[hi] - self.times[lo];
        let f = if span > 0.0 {
            (t - self.times[lo]) / span
        } else {
            1.0
        };
        let (a, b) = (key(lo), key(hi));
        // Shortest path: a quaternion and its negation are one rotation.
        let sign = if width == 4 && a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() < 0.0 {
            -1.0
        } else {
            1.0
        };
        let mut v: Vec<f32> = a.iter().zip(b).map(|(x, y)| x + (y * sign - x) * f).collect();
        if width == 4 {
            let len = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if len > 0.0 {
                v.iter_mut().for_each(|x| *x /= len);
            }
        }
        v
    }
}

impl SampledClip {
    /// The frames of the clip at time `t`, one per animated item (nodes as `transform`, skins as
    /// `pose`).
    #[must_use]
    pub fn frames_at(&self, scene: &FeedScene, t: f32, frames: &mut FrameIds) -> Vec<Message> {
        let apply =
            |node: usize, t3: &mut [f32; 3], r4: &mut [f32; 4], s3: &mut [f32; 3], touched: &mut bool| {
                for track in self.tracks.iter().filter(|k| k.node == node) {
                    let value = track.value(t);
                    match (track.property, value.as_slice()) {
                        (0, [x, y, z]) => *t3 = [*x, *y, *z],
                        (1, [x, y, z, w]) => *r4 = [*x, *y, *z, *w],
                        (2, [x, y, z]) => *s3 = [*x, *y, *z],
                        _ => continue,
                    }
                    *touched = true;
                }
            };
        let mut out = Vec::new();
        for item in &scene.items {
            match &item.data {
                ItemData::Skeleton(joints, sources) => {
                    let mut posed = joints.clone();
                    let mut touched = false;
                    for (joint, source) in posed.iter_mut().zip(sources) {
                        if source.chain.is_empty() {
                            apply(
                                source.node,
                                &mut joint.t,
                                &mut joint.r,
                                &mut joint.s,
                                &mut touched,
                            );
                            continue;
                        }
                        // The skipped nodes may be animated too: pose each, then fold again.
                        let mut moved = false;
                        let mut pose = |node: usize, rest: &TransformBlock| {
                            let mut xf = *rest;
                            apply(
                                node,
                                &mut xf.translation,
                                &mut xf.rotation,
                                &mut xf.scale,
                                &mut moved,
                            );
                            xf
                        };
                        let local = pose(source.node, &source.local);
                        let chain: Vec<TransformBlock> = source
                            .chain
                            .iter()
                            .map(|(node, rest)| pose(*node, rest))
                            .collect();
                        if moved {
                            let folded = fold_chain(chain.iter(), local);
                            (joint.t, joint.r, joint.s) = (folded.translation, folded.rotation, folded.scale);
                            touched = true;
                        }
                    }
                    if touched {
                        out.push(pose_of(&posed, item.declare.handle, frames));
                    }
                }
                _ => {
                    let mut moved = item.clone();
                    let mut touched = false;
                    apply(
                        item.node,
                        &mut moved.xf.translation,
                        &mut moved.xf.rotation,
                        &mut moved.xf.scale,
                        &mut touched,
                    );
                    if touched {
                        out.push(placement_frame(&moved, frames));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------------------------

enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Stream {
    fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Self::Tcp(s) => s.try_clone().map(Self::Tcp),
            #[cfg(unix)]
            Self::Unix(s) => s.try_clone().map(Self::Unix),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
        }
    }
}

/// Where to connect, and whether the transport needs the token.
fn resolve_endpoint(args: &SendArgs) -> Result<(String, bool)> {
    if let Some(endpoint) = &args.endpoint {
        return Ok((endpoint.clone(), !endpoint.starts_with("unix:")));
    }
    let path = regia_agent::dcc_feed::net::feed_dir(&args.project).join("endpoint.json");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read {}: is the feed enabled in a running app?",
            path.display()
        )
    })?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
    let transports = doc["transports"].as_array().cloned().unwrap_or_default();
    let address = |name: &str| {
        transports
            .iter()
            .find(|t| t["transport"] == name)
            .and_then(|t| t["address"].as_str())
            .map(str::to_owned)
    };
    if cfg!(unix)
        && let Some(path) = address("unix")
    {
        return Ok((format!("unix:{path}"), false));
    }
    match address("tcp") {
        Some(addr) => Ok((format!("tcp:{addr}"), true)),
        None => bail!("{} lists no transport this client can use", path.display()),
    }
}

fn connect(endpoint: &str) -> Result<Stream> {
    #[cfg(unix)]
    if let Some(path) = endpoint.strip_prefix("unix:") {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("cannot connect to {path}"))?;
        return Ok(Stream::Unix(stream));
    }
    let addr = endpoint.strip_prefix("tcp:").unwrap_or(endpoint);
    let stream = TcpStream::connect(addr).with_context(|| format!("cannot connect to {addr}"))?;
    stream.set_nodelay(true)?;
    Ok(Stream::Tcp(stream))
}

/// A connected sender: writes frames within the server's byte window.
struct Session {
    out: Stream,
    seq: u32,
    window: u64,
    unapplied: u64,
    replies: Receiver<Message>,
    record: Option<Recording>,
    started: Instant,
    /// The `sim_tick` of records: the client has no simulation, so one tick per clip step or
    /// `--watch` change.
    tick: u64,
}

impl Session {
    fn write(&mut self, msg: &Message) -> Result<()> {
        let mut frame = Vec::new();
        // `encode_fragments` returns the next unused seq; a recording keeps the seq the frame used.
        let first_seq = self.seq;
        self.seq = wire::encode_fragments(msg, self.seq, MAX_FRAGMENT, &mut frame)
            .map_err(|e| anyhow::anyhow!("cannot encode a {:?} message: {e}", msg.msg_type()))?;
        // A goodbye ends the session, it is not data: a replay would drop the last tick's frames.
        if let Some(record) = self
            .record
            .as_mut()
            .filter(|_| !matches!(msg, Message::Goodbye(_)))
        {
            let mut whole = Vec::new();
            // A recording gets shared; the session token stays out of it.
            let encoded = match msg {
                Message::Hello(hello) => {
                    let hello = Hello {
                        token: String::new(),
                        ..hello.clone()
                    };
                    wire::encode(&Message::Hello(hello), first_seq, &mut whole)
                }
                _ => wire::encode(msg, first_seq, &mut whole),
            };
            if encoded.is_ok() {
                record.push(self.tick, whole);
            }
        }
        self.out.write_all(&frame).context("the connection closed")?;
        Ok(())
    }

    /// Writes a data message, first waiting for credits when the window is full (spec § 4.7).
    fn send(&mut self, msg: &Message) -> Result<()> {
        let mut encoded = Vec::new();
        wire::encode(msg, 0, &mut encoded).map_err(|e| anyhow::anyhow!("{e}"))?;
        let size = encoded.len() as u64;
        while self.unapplied > 0 && self.unapplied + size > self.window {
            self.pump(Some(DRAIN_TIMEOUT))?;
        }
        self.unapplied += size;
        self.write(msg)
    }

    /// Handles one reply (waiting up to `wait`, or none); answers what needs an answer.
    fn pump(&mut self, wait: Option<Duration>) -> Result<Option<Message>> {
        let next = match wait {
            Some(wait) => self.replies.recv_timeout(wait),
            None => self.replies.recv_timeout(Duration::ZERO),
        };
        let msg = match next {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) if wait.is_some() => bail!("the server did not answer in time"),
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("the server closed the connection"),
        };
        match &msg {
            Message::Ack(ack) => self.unapplied = self.unapplied.saturating_sub(u64::from(ack.credit_bytes)),
            Message::Error(e) => {
                eprintln!(
                    "regia dcc-feed: server error {} (handle {}): {}",
                    e.code, e.handle, e.message
                );
                if e.fatal {
                    bail!("the server closed the session: {}", e.code);
                }
            }
            _ => {}
        }
        Ok(Some(msg))
    }

    fn now_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Writes the recording so far, when `--record` asked for one.
    fn save_recording(&self, path: Option<&Path>) -> Result<()> {
        if let (Some(path), Some(record)) = (path, &self.record) {
            record
                .write_file(path)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        }
        Ok(())
    }
}

/// Connects, says hello and waits until the session is accepted.
fn open(args: &SendArgs) -> Result<Session> {
    let (endpoint, needs_token) = resolve_endpoint(args)?;
    let token = if needs_token {
        let path = args
            .token_file
            .clone()
            .unwrap_or_else(|| regia_agent::dcc_feed::net::feed_dir(&args.project).join("token"));
        std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read the token {}", path.display()))?
            .trim()
            .to_owned()
    } else {
        String::new()
    };
    let stream = connect(&endpoint)?;
    let reader = stream.try_clone()?;
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("dcc-feed-replies".into())
        .spawn(move || read_replies(reader, &tx))?;
    let mut session = Session {
        out: stream,
        seq: 1,
        window: 32 << 20,
        unapplied: 0,
        replies: rx,
        record: args
            .record
            .as_ref()
            .map(|_| Recording::new(args.client_name.clone(), "regia dcc-feed")),
        started: Instant::now(),
        tick: 0,
    };
    session.write(&Message::Hello(Hello {
        min_version: PROTOCOL_VERSION,
        max_version: PROTOCOL_VERSION,
        client_name: args.client_name.clone(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        dcc: "regia dcc-feed".to_owned(),
        space: "gltf".to_owned(),
        unit_scale: 1.0,
        caps: 0b0_1111,
        token,
    }))?;
    loop {
        match session.pump(Some(Duration::from_secs(130)))? {
            Some(Message::HelloAck(ack)) => {
                session.window = u64::from(ack.window_bytes);
                if !ack.accepted {
                    println!(
                        "session {} is pending: accept it in the editor or with dcc_feed.accept",
                        ack.session_id
                    );
                }
            }
            // A `resync` follows the accept: the server is ready for the full state.
            Some(Message::Resync(_)) => return Ok(session),
            _ => {}
        }
    }
}

fn read_replies(stream: Stream, tx: &std::sync::mpsc::Sender<Message>) {
    let limits = Limits::default();
    let mut reader = FrameReader::new(stream);
    let mut assembler = Reassembler::new();
    while let Ok(Some((header, payload))) = reader.next(&limits) {
        let Ok(Some((head, body))) = assembler.push(header, payload, &limits) else {
            continue;
        };
        if let Ok(msg) = wire::decode(head, &body, &limits)
            && tx.send(msg).is_err()
        {
            return;
        }
    }
}

fn send(args: &SendArgs) -> Result<()> {
    let mut scene = load_scene(&args.file)?;
    let mut session = open(args)?;
    let mut frames = FrameIds {
        time_ns: session.now_ns(),
        ..FrameIds::default()
    };
    for msg in scene.full_state(&mut frames) {
        session.send(&msg)?;
    }
    println!("sent {} items from {}", scene.items.len(), args.file.display());
    if let Some(name) = &args.animate {
        animate(&mut session, &scene, name, args.fps, &mut frames)?;
    }
    if args.watch {
        return watch(&mut session, &mut scene, args, &mut frames);
    }
    drain(&mut session)?;
    session.write(&Message::Goodbye("done".to_owned()))?;
    session.save_recording(args.record.as_deref())?;
    if let (Some(path), Some(record)) = (&args.record, &session.record) {
        println!("recorded {} frames to {}", record.records.len(), path.display());
    }
    Ok(())
}

// Every integer index through 2^24 is exactly representable by the f32 sampling clock.
fn animation_sample_count(duration_s: f32, fps: u32) -> Result<u32> {
    if !duration_s.is_finite() || duration_s < 0.0 {
        bail!("animation_samples_invalid: duration must be finite and nonnegative");
    }
    let step = 1.0 / fps.max(1) as f32;
    let count = (duration_s / step).ceil();
    const MAX_EXACT_SAMPLES: f32 = (1u32 << f32::MANTISSA_DIGITS) as f32;
    if !count.is_finite() || count > MAX_EXACT_SAMPLES {
        bail!("animation_samples_invalid: clip exceeds {MAX_EXACT_SAMPLES} exactly indexed sample steps");
    }
    Ok(count as u32)
}

fn animate(
    session: &mut Session,
    scene: &FeedScene,
    name: &str,
    fps: u32,
    frames: &mut FrameIds,
) -> Result<()> {
    let Some(clip) = scene.clips.iter().find(|c| c.name == name) else {
        let known: Vec<&str> = scene.clips.iter().map(|c| c.name.as_str()).collect();
        bail!("no animation {name:?} in the file (it has: {})", known.join(", "));
    };
    let step = 1.0 / fps.max(1) as f32;
    let count = animation_sample_count(clip.duration_s, fps)?;
    for i in 0..=count {
        session.tick += 1;
        frames.time_ns = session.now_ns();
        for msg in clip.frames_at(scene, i as f32 * step, frames) {
            session.send(&msg)?;
        }
        while session.pump(None)?.is_some() {}
        std::thread::sleep(Duration::from_secs_f32(step));
    }
    Ok(())
}

/// Runs until the connection ends, so a `--record` file is rewritten after each change: stopping
/// the client (Ctrl-C) loses nothing.
fn watch(session: &mut Session, scene: &mut FeedScene, args: &SendArgs, frames: &mut FrameIds) -> Result<()> {
    let file = args.file.as_path();
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut stamp = modified(file);
    println!("watching {}", file.display());
    session.save_recording(args.record.as_deref())?;
    loop {
        while let Some(msg) = session.pump(None)? {
            if matches!(msg, Message::Resync(_)) {
                frames.time_ns = session.now_ns();
                for full in scene.full_state(frames) {
                    session.send(&full)?;
                }
            }
        }
        let now = modified(file);
        if now != stamp {
            stamp = now;
            match load_scene(file) {
                Ok(next) => {
                    let next = adopt_handles(scene, &next);
                    session.tick += 1;
                    frames.time_ns = session.now_ns();
                    let changes = diff_scenes(scene, &next, frames);
                    println!("{} changes", changes.len());
                    for msg in &changes {
                        session.send(msg)?;
                    }
                    *scene = next;
                    session.save_recording(args.record.as_deref())?;
                }
                // An exporter may be half way through writing; the next change is read again.
                Err(err) => eprintln!("regia dcc-feed: {err:#}"),
            }
        }
        std::thread::sleep(WATCH_POLL);
    }
}

/// Waits until the server applied everything sent.
fn drain(session: &mut Session) -> Result<()> {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while session.unapplied > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("the server did not apply {} bytes in time", session.unapplied);
        }
        session.pump(Some(left))?;
    }
    Ok(())
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    #[test]
    fn audit_texture_uris_decode_external_paths_and_embedded_payloads() {
        let dir = regia_test_utils::unique_temp_dir("dcc-uri-images");
        std::fs::create_dir_all(dir.join("textures")).expect("textures");
        std::fs::write(dir.join("textures/albedo base.png"), b"external encoded bytes").expect("external");
        let document = gltf::Gltf::from_slice(
            br#"{
            "asset":{"version":"2.0"},
            "images":[{"uri":"textures/albedo%20base.png"},{"uri":"data:image/png;base64,AQID"}]
        }"#,
        )
        .expect("glTF image references")
        .document;
        let textures = texture_pushes(&dir.join("model.gltf"), &document, &[]);
        assert_eq!(textures.len(), 2);
        for (texture, expected) in textures
            .iter()
            .zip([b"external encoded bytes".as_slice(), &[1, 2, 3]])
        {
            let TextureSource::Encoded { container, bytes } = &texture.source else {
                panic!("encoded input")
            };
            assert_eq!(*container, 0);
            assert_eq!(bytes, expected);
        }
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn audit_animation_rejects_nonfinite_and_inexact_sample_counts() {
        assert_eq!(animation_sample_count(2.0, 30).expect("ordinary clip"), 60);
        assert_eq!(animation_sample_count(0.0, 30).expect("single pose"), 0);
        for duration in [f32::INFINITY, f32::NAN, -1.0, 1.0e10] {
            assert!(animation_sample_count(duration, 30).is_err(), "{duration}");
        }
        assert!(animation_sample_count((1u32 << 24) as f32 + 2.0, 1).is_err());
    }

    #[test]
    fn audit_step_uses_the_key_at_an_exact_interior_time() {
        let track = Track {
            node: 0,
            property: 0,
            times: vec![0.0, 1.0, 2.0],
            values: vec![0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 20.0, 0.0, 0.0],
            step: true,
        };
        assert_eq!(track.value(0.5), vec![0.0, 0.0, 0.0]);
        assert_eq!(track.value(1.0), vec![10.0, 0.0, 0.0]);
        assert_eq!(track.value(1.5), vec![10.0, 0.0, 0.0]);
    }

    #[test]
    fn cubic_spline_keeps_only_the_vertex_of_each_triple() {
        // Two keys of a vec3 track: (in, value, out) each.
        let v = [
            9.0, 9.0, 9.0, 1.0, 2.0, 3.0, 8.0, 8.0, 8.0, 7.0, 7.0, 7.0, 4.0, 5.0, 6.0, 6.0, 6.0, 6.0,
        ];
        assert_eq!(
            strip_cubic_tangents(&v, 3, 2),
            Some(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
        );
        // A count that disagrees with the key count is refused instead of desyncing the wire.
        assert_eq!(strip_cubic_tangents(&v[..8], 3, 2), None);
    }

    /// CLI2 audit 3, beyond `audit_step_uses_the_key_at_an_exact_interior_time`: the first and
    /// last keys, and keys on the 30 fps grid `--animate` samples, each show their own key.
    #[test]
    fn step_tracks_hold_the_key_at_an_exact_key_time() {
        let track = Track {
            node: 0,
            property: 0,
            times: vec![0.0, 1.0, 2.0],
            values: vec![0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 20.0, 0.0, 0.0],
            step: true,
        };
        assert_eq!(track.value(0.0), vec![0.0, 0.0, 0.0]);
        assert_eq!(track.value(0.5), vec![0.0, 0.0, 0.0]);
        assert_eq!(track.value(1.0), vec![10.0, 0.0, 0.0], "on the key: that key");
        assert_eq!(track.value(1.5), vec![10.0, 0.0, 0.0]);
        assert_eq!(track.value(2.0), vec![20.0, 0.0, 0.0]);
        // Keys on the 30 fps grid that `--animate` samples: every sample shows its own key.
        let grid = Track {
            node: 0,
            property: 0,
            times: (0..4u8).map(|i| f32::from(i) / 30.0).collect(),
            values: (0..4u8).flat_map(|i| [f32::from(i), 0.0, 0.0]).collect(),
            step: true,
        };
        for i in 0..4u8 {
            assert_eq!(grid.value(f32::from(i) / 30.0)[0], f32::from(i), "sample {i}");
        }
    }

    /// CLI2 audit 10 through `load_scene` (the path `dcc-feed send` takes, including the
    /// importer's URI check): a percent-encoded external image and a `data:` image both arrive.
    #[test]
    fn texture_uris_are_percent_decoded_and_data_uris_are_read() {
        use base64::Engine as _;
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_texture_uris");
        let on_disk = b"\x89PNG on disk".to_vec();
        std::fs::write(dir.join("albedo base.png"), &on_disk).expect("texture");
        let inline = b"\x89PNG inline".to_vec();
        let data = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&inline)
        );
        let doc = serde_json::json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "name": "N" }],
            "images": [{ "uri": "albedo%20base.png" }, { "uri": data }],
        });
        let path = dir.join("scene.gltf");
        std::fs::write(&path, doc.to_string()).expect("gltf");
        let scene = load_scene(&path).expect("loads");
        let bytes: Vec<(u8, Vec<u8>)> = scene
            .textures
            .iter()
            .map(|t| match &t.source {
                TextureSource::Encoded { container, bytes } => (*container, bytes.clone()),
                other => panic!("unexpected source {other:?}"),
            })
            .collect();
        assert_eq!(bytes, vec![(0, on_disk), (0, inline)], "both images are pushed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn texture_containers_follow_the_spec_codes() {
        assert_eq!(container_of(mime_of_uri("a/Base.PNG")), 0);
        assert_eq!(container_of(mime_of_uri("a.jpg")), 1);
        assert_eq!(container_of(mime_of_uri("a.exr")), 2);
        assert_eq!(container_of(mime_of_uri("a.ktx2")), 3);
        assert_eq!(container_of("image/jpeg"), 1);
    }

    /// Parent scale (2, 1, 1) and a child rotation of +90° about Z is an exact TRS: child +X
    /// lands on +Y (length 1) and child +Y lands on -X (length 2). Multiplying the scale
    /// components stores (2, 1, 1) and sends +X to (0, 2, 0).
    #[test]
    fn audfix2_compose_rotates_nonuniform_parent_scale() {
        let parent = block(([0.0; 3], [0.0, 0.0, 0.0, 1.0], [2.0, 1.0, 1.0]));
        let half = std::f32::consts::FRAC_1_SQRT_2;
        let child = block(([0.0; 3], [0.0, 0.0, half, half], [1.0, 1.0, 1.0]));
        let got = compose(&parent, &child);
        let apply = |p: [f32; 3]| {
            let scaled = [got.scale[0] * p[0], got.scale[1] * p[1], got.scale[2] * p[2]];
            let rotated = quat_rotate(got.rotation, scaled);
            [
                rotated[0] + got.translation[0],
                rotated[1] + got.translation[1],
                rotated[2] + got.translation[2],
            ]
        };
        let x = apply([1.0, 0.0, 0.0]);
        let y = apply([0.0, 1.0, 0.0]);
        assert!(
            x.iter().zip([0.0, 1.0, 0.0]).all(|(a, b)| (a - b).abs() < 1.0e-5),
            "+X should map to (0, 1, 0), got {x:?} scale {:?} rot {:?}",
            got.scale,
            got.rotation
        );
        assert!(
            y.iter()
                .zip([-2.0, 0.0, 0.0])
                .all(|(a, b)| (a - b).abs() < 1.0e-5),
            "+Y should map to (-2, 0, 0), got {y:?}"
        );
    }

    /// A cycle among the non-joint ancestors of a joint is invalid glTF. It was folded after one
    /// lap; the checked parse now refuses it outright (the walk never sees a cycle).
    #[test]
    fn audfix2_joint_cycle_is_folded_once() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_joint_cycle");
        let doc = serde_json::json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "name": "Joint" },
                { "name": "A", "translation": [1.0, 0.0, 0.0], "children": [0, 2] },
                { "name": "B", "translation": [10.0, 0.0, 0.0], "children": [1] },
                { "name": "Pad0" },
                { "name": "Pad1" },
                { "name": "Pad2" },
                { "name": "Pad3" },
                { "name": "Pad4" },
            ],
            "skins": [{ "name": "Rig", "joints": [0] }],
        });
        let path = dir.join("cycle.gltf");
        std::fs::write(&path, doc.to_string()).expect("gltf");
        // Since integ-v049 the feed reads glTF through the checked parse (sec-review-assets-agent,
        // ADR-5923 (3)), which refuses a node cycle (glTF 2.0 § 3.5.3) before any walk: the file
        // is refused with a cycle error instead of folded, so no walk can loop (ADR-5993).
        let error = match load_scene(&path) {
            Ok(_) => panic!("a node cycle must be refused"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("cycle"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
