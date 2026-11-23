// SPDX-License-Identifier: MPL-2.0
//! The DCC feed wire protocol v1 (`specs/60` § 4, `docs/formats/dcc-feed-protocol-v1.md`).
//!
//! All integers are little-endian, floats are IEEE 754 binary32, strings are a `u16` byte length
//! plus UTF-8. [`decode`] is pure and total: no byte string panics it, and every length is
//! checked against [`Limits`] and the bytes actually present before anything is allocated.

use std::io::Read;

use regia_ecs::prelude::*;

/// The protocol major version this module speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Magic at the start of a `hello` payload.
pub const HELLO_MAGIC: [u8; 4] = *b"RFDP";
/// Size of a [`FrameHeader`] on the wire (`len` included).
pub const HEADER_BYTES: usize = 12;
/// Header flag: the payload continues in the next frame of the same message type.
pub const FLAG_FRAGMENT_MORE: u16 = 1;
/// Header flag: a receiver may ignore an unknown message type.
pub const FLAG_OPTIONAL: u16 = 2;
/// Header flag: reserved, must be 0 in v1.
pub const FLAG_COMPRESSED: u16 = 4;

/// Message type ids (spec § 4.4).
pub mod msg {
    /// `hello`
    pub const HELLO: u16 = 0x0001;
    /// `hello_ack`
    pub const HELLO_ACK: u16 = 0x0081;
    /// `item_declare`
    pub const ITEM_DECLARE: u16 = 0x0010;
    /// `item_delete`
    pub const ITEM_DELETE: u16 = 0x0011;
    /// `transform`
    pub const TRANSFORM: u16 = 0x0020;
    /// `camera`
    pub const CAMERA: u16 = 0x0021;
    /// `light`
    pub const LIGHT: u16 = 0x0022;
    /// `mesh`
    pub const MESH: u16 = 0x0023;
    /// `skeleton_static`
    pub const SKELETON_STATIC: u16 = 0x0024;
    /// `pose`
    pub const POSE: u16 = 0x0025;
    /// `clip_push`
    pub const CLIP_PUSH: u16 = 0x0026;
    /// `texture_push`
    pub const TEXTURE_PUSH: u16 = 0x0027;
    /// `ping`
    pub const PING: u16 = 0x0030;
    /// `pong`
    pub const PONG: u16 = 0x0031;
    /// `goodbye`
    pub const GOODBYE: u16 = 0x0032;
    /// `error`
    pub const ERROR: u16 = 0x003F;
    /// `ack`
    pub const ACK: u16 = 0x0082;
    /// `session_state`
    pub const SESSION_STATE: u16 = 0x0083;
    /// `resync`
    pub const RESYNC: u16 = 0x0084;
}

/// Mesh section ids (spec § 4.5).
pub mod section {
    /// Vertex positions, 3 x f32.
    pub const POSITIONS: u8 = 1;
    /// Unit normals, 3 x f32.
    pub const NORMALS: u8 = 2;
    /// Tangents xyz + sign, 4 x f32.
    pub const TANGENTS: u8 = 3;
    /// Texture coordinates, 2 x f32, set `index` 0 to 3.
    pub const UV: u8 = 4;
    /// Vertex color, 4 x f32 or 4 x u8 normalized.
    pub const COLOR: u8 = 5;
    /// Triangle list, u16 or u32.
    pub const INDICES: u8 = 6;
    /// Draw ranges, 3 x u32 each.
    pub const SUBMESHES: u8 = 7;
    /// Material slot table.
    pub const MATERIAL_SLOTS: u8 = 8;
    /// Skin joint indices, 4 x u16.
    pub const JOINTS: u8 = 9;
    /// Skin weights, 4 x f32.
    pub const WEIGHTS: u8 = 10;
}

/// Size limits (spec § 4.8), sent in `hello_ack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest reassembled message payload in bytes.
    pub max_message_bytes: u32,
    /// Most vertices in one mesh.
    pub max_vertices: u32,
    /// Most indices in one mesh.
    pub max_indices: u32,
    /// Most items per session.
    pub max_items: u32,
    /// Most joints in a skeleton.
    pub max_joints: u32,
    /// Most tracks in a clip.
    pub max_clip_tracks: u32,
    /// Most keys in one track.
    pub max_keys_per_track: u32,
    /// Largest texture edge in texels.
    pub max_texture_dim: u32,
    /// Largest single texture push in bytes.
    pub max_texture_bytes: u32,
    /// Total transient texture bytes (least recently used evicted).
    pub max_transient_texture_bytes: u64,
    /// Longest item key in bytes.
    pub max_key_len: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_bytes: 64 << 20,
            max_vertices: 2_000_000,
            max_indices: 12_000_000,
            max_items: 2048,
            max_joints: 1024,
            max_clip_tracks: 4096,
            max_keys_per_track: 100_000,
            max_texture_dim: 8192,
            max_texture_bytes: 256 << 20,
            max_transient_texture_bytes: 512 << 20,
            max_key_len: 128,
        }
    }
}

/// Why a frame was refused. The lowercase code is shared with the agent protocol (spec § 4.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The hello magic is wrong.
    BadMagic,
    /// No common protocol version.
    UnsupportedVersion,
    /// Missing or wrong token.
    AuthFailed,
    /// The session is not accepted yet.
    NotAccepted,
    /// A frame is larger than the limit.
    FrameTooLarge,
    /// A count or size exceeds a limit.
    LimitExceeded,
    /// The bytes do not parse.
    MalformedFrame,
    /// A mandatory message type is unknown.
    UnsupportedMessage,
    /// A frame addressed an undeclared handle.
    UnknownItem,
    /// Another session owns the key.
    ItemConflict,
    /// The key exists with another kind.
    KindMismatch,
    /// The kind is not built into this receiver.
    KindNotSupported,
    /// A vertex-only update changed the vertex count.
    TopologyMismatch,
    /// A float is NaN or infinite, or a value is out of range.
    InvalidValue,
    /// A texture path leaves the allowed directories.
    PathDenied,
    /// The sender exceeded its byte window.
    Overloaded,
    /// A deadline passed.
    Timeout,
    /// A receiver bug.
    Internal,
}

impl ErrorCode {
    /// Every code, in the order of spec § 4.10.
    pub const ALL: [ErrorCode; 18] = [
        Self::BadMagic,
        Self::UnsupportedVersion,
        Self::AuthFailed,
        Self::NotAccepted,
        Self::FrameTooLarge,
        Self::LimitExceeded,
        Self::MalformedFrame,
        Self::UnsupportedMessage,
        Self::UnknownItem,
        Self::ItemConflict,
        Self::KindMismatch,
        Self::KindNotSupported,
        Self::TopologyMismatch,
        Self::InvalidValue,
        Self::PathDenied,
        Self::Overloaded,
        Self::Timeout,
        Self::Internal,
    ];

    /// The stable lowercase code (`[a-z0-9_]+`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadMagic => "bad_magic",
            Self::UnsupportedVersion => "unsupported_version",
            Self::AuthFailed => "auth_failed",
            Self::NotAccepted => "not_accepted",
            Self::FrameTooLarge => "frame_too_large",
            Self::LimitExceeded => "limit_exceeded",
            Self::MalformedFrame => "malformed_frame",
            Self::UnsupportedMessage => "unsupported_message",
            Self::UnknownItem => "unknown_item",
            Self::ItemConflict => "item_conflict",
            Self::KindMismatch => "kind_mismatch",
            Self::KindNotSupported => "kind_not_supported",
            Self::TopologyMismatch => "topology_mismatch",
            Self::InvalidValue => "invalid_value",
            Self::PathDenied => "path_denied",
            Self::Overloaded => "overloaded",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
        }
    }

    /// Parses a wire code back; `None` for a code this version does not know.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == code)
    }

    /// Whether the session closes after this error is sent (spec § 4.10).
    #[must_use]
    pub fn is_fatal(self) -> bool {
        matches!(
            self,
            Self::BadMagic
                | Self::UnsupportedVersion
                | Self::AuthFailed
                | Self::FrameTooLarge
                | Self::MalformedFrame
                | Self::UnsupportedMessage
                | Self::Internal
        )
    }
}

/// A codec failure: a code plus a human-readable detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct WireError {
    /// The shared error code.
    pub code: ErrorCode,
    /// Detail for logs and the `error` message.
    pub message: String,
}

impl WireError {
    /// An error with `code` and `message`.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The stable lowercase code (spec § 4.10).
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }
}

fn malformed(message: &str) -> WireError {
    WireError::new(ErrorCode::MalformedFrame, message)
}

/// The 12-byte frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Bytes after this field: 8 + payload length.
    pub len: u32,
    /// Message type id (see [`msg`]).
    pub msg_type: u16,
    /// Flag bits (`FLAG_*`).
    pub flags: u16,
    /// Per-direction counter starting at 1.
    pub seq: u32,
}

impl FrameHeader {
    /// Payload length implied by `len`, or `None` when `len` is below the 8 header bytes.
    #[must_use]
    pub fn payload_len(&self) -> Option<usize> {
        (self.len as usize).checked_sub(8)
    }

    /// Parses 12 header bytes.
    #[must_use]
    pub fn parse(bytes: &[u8; HEADER_BYTES]) -> Self {
        Self {
            len: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            msg_type: u16::from_le_bytes([bytes[4], bytes[5]]),
            flags: u16::from_le_bytes([bytes[6], bytes[7]]),
            seq: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        }
    }

    /// The 12 header bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; HEADER_BYTES] {
        let mut out = [0u8; HEADER_BYTES];
        out[0..4].copy_from_slice(&self.len.to_le_bytes());
        out[4..6].copy_from_slice(&self.msg_type.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&self.seq.to_le_bytes());
        out
    }
}

/// What an item's data drives (spec § 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
pub enum FeedKind {
    /// A plain transform.
    Transform = 1,
    /// A transform plus geometry.
    Mesh = 2,
    /// A transform plus lens.
    Camera = 3,
    /// A transform plus emitter.
    Light = 4,
    /// Static joint list plus per-frame poses.
    Skeleton = 5,
    /// Named float channels.
    Generic = 6,
}

impl FeedKind {
    /// Parses the wire byte.
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Transform,
            2 => Self::Mesh,
            3 => Self::Camera,
            4 => Self::Light,
            5 => Self::Skeleton,
            6 => Self::Generic,
            _ => return None,
        })
    }

    /// Lowercase name used by the agent surface.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Transform => "transform",
            Self::Mesh => "mesh",
            Self::Camera => "camera",
            Self::Light => "light",
            Self::Skeleton => "skeleton",
            Self::Generic => "generic",
        }
    }
}

/// `hello` (client to server).
#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    /// Lowest protocol version the client speaks.
    pub min_version: u32,
    /// Highest protocol version the client speaks.
    pub max_version: u32,
    /// Client name (allow-list key).
    pub client_name: String,
    /// Client version text.
    pub client_version: String,
    /// Free text: host application name and version.
    pub dcc: String,
    /// `gltf` or `regia`.
    pub space: String,
    /// Multiplies positions, translations and lens distances.
    pub unit_scale: f32,
    /// Capability bits (bit0 mesh, bit1 texture, bit2 clip, bit3 pose, bit4 text frames).
    pub caps: u32,
    /// Session token; empty on a Unix socket.
    pub token: String,
}

/// `hello_ack` (server to client).
#[derive(Debug, Clone, PartialEq)]
pub struct HelloAck {
    /// Chosen protocol version.
    pub version: u32,
    /// Session id.
    pub session_id: u64,
    /// Whether the session is `active` already.
    pub accepted: bool,
    /// Capability bits the server supports.
    pub caps: u32,
    /// Limits in force.
    pub limits: Limits,
    /// Byte window for unapplied frames.
    pub window_bytes: u32,
    /// Server name.
    pub server_name: String,
}

/// `item_declare`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemDeclare {
    /// Session-local handle (non-zero).
    pub handle: u32,
    /// Stable key (identity).
    pub key: String,
    /// Kind.
    pub kind: FeedKind,
    /// Parent handle, 0 for none.
    pub parent_handle: u32,
    /// Display name.
    pub display_name: String,
    /// Light: 0 directional, 1 point, 2 spot. Camera: 0 perspective, 1 orthographic.
    pub variant: u8,
    /// Generic items: channel names.
    pub channels: Vec<String>,
}

/// Timecode carried by a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    /// Hours.
    pub hh: u8,
    /// Minutes.
    pub mm: u8,
    /// Seconds.
    pub ss: u8,
    /// Frames.
    pub ff: u8,
    /// Rate numerator.
    pub rate_num: u16,
    /// Rate denominator.
    pub rate_den: u16,
    /// Drop-frame flag.
    pub drop_frame: bool,
}

/// Per-frame header of the data messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Strictly increasing per item.
    pub frame_id: u64,
    /// Sender monotonic clock in nanoseconds.
    pub source_time_ns: u64,
    /// Optional timecode.
    pub timecode: Option<Timecode>,
}

/// Translation, rotation and scale of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformBlock {
    /// Translation.
    pub translation: [f32; 3],
    /// Unit quaternion x, y, z, w.
    pub rotation: [f32; 4],
    /// Scale.
    pub scale: [f32; 3],
    /// True when the values are world space instead of parent-local.
    pub world_space: bool,
}

/// `transform`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformFrame {
    /// Item handle.
    pub handle: u32,
    /// Frame info.
    pub info: FrameInfo,
    /// The transform.
    pub xf: TransformBlock,
}

/// Lens of a camera frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lens {
    /// Perspective projection.
    Perspective {
        /// Vertical field of view in radians.
        yfov_rad: f32,
        /// Aspect ratio, 0 = viewport.
        aspect: f32,
        /// Near plane.
        near: f32,
        /// Far plane, 0 = infinite.
        far: f32,
    },
    /// Orthographic projection.
    Orthographic {
        /// Horizontal half extent.
        xmag: f32,
        /// Vertical half extent.
        ymag: f32,
        /// Near plane.
        near: f32,
        /// Far plane.
        far: f32,
    },
}

/// Physical camera block; absent when every field is 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicalCamera {
    /// Focal length in mm.
    pub focal_mm: f32,
    /// Sensor width in mm.
    pub sensor_w_mm: f32,
    /// Sensor height in mm.
    pub sensor_h_mm: f32,
    /// Focus distance in meters.
    pub focus_m: f32,
    /// F-stop.
    pub f_stop: f32,
}

/// `camera`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraFrame {
    /// Item handle.
    pub handle: u32,
    /// Frame info.
    pub info: FrameInfo,
    /// Placement.
    pub xf: TransformBlock,
    /// Lens.
    pub lens: Lens,
    /// Physical block, if any field was non-zero.
    pub physical: Option<PhysicalCamera>,
}

/// `light`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LightFrame {
    /// Item handle.
    pub handle: u32,
    /// Frame info.
    pub info: FrameInfo,
    /// Placement.
    pub xf: TransformBlock,
    /// Linear RGB.
    pub color: [f32; 3],
    /// glTF punctual-light units.
    pub intensity: f32,
    /// Range, 0 = none.
    pub range: f32,
    /// Spot inner cone angle in radians.
    pub spot_inner_rad: f32,
    /// Spot outer cone angle in radians.
    pub spot_outer_rad: f32,
}

/// Element type of a mesh section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elem {
    /// 32-bit float.
    F32 = 0,
    /// Normalized unsigned byte.
    U8 = 1,
    /// Unsigned 16-bit.
    U16 = 2,
    /// Unsigned 32-bit.
    U32 = 3,
}

impl Elem {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::F32,
            1 => Self::U8,
            2 => Self::U16,
            3 => Self::U32,
            _ => return None,
        })
    }

    /// Bytes per element.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            Self::F32 | Self::U32 => 4,
            Self::U16 => 2,
            Self::U8 => 1,
        }
    }
}

/// One section of a `mesh` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshSection {
    /// Section id (see [`section`]); unknown ids are kept and skipped by receivers.
    pub section_id: u8,
    /// Element type.
    pub elem: Elem,
    /// UV or color set number, else 0.
    pub index: u16,
    /// Number of items (vertices, indices, ranges or slots), not scalar elements.
    pub count: u32,
    /// Raw little-endian bytes.
    pub data: Vec<u8>,
}

/// `mesh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshUpdate {
    /// Item handle.
    pub handle: u32,
    /// Frame info.
    pub info: FrameInfo,
    /// 0 full, 1 vertex-only.
    pub update_mode: u8,
    /// The sections.
    pub sections: Vec<MeshSection>,
}

/// One joint of a static skeleton.
#[derive(Debug, Clone, PartialEq)]
pub struct JointStatic {
    /// Joint name.
    pub name: String,
    /// Parent joint index, -1 for a root.
    pub parent: i32,
    /// Bind translation.
    pub t: [f32; 3],
    /// Bind rotation.
    pub r: [f32; 4],
    /// Bind scale.
    pub s: [f32; 3],
}

/// `skeleton_static`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkeletonStatic {
    /// Item handle.
    pub handle: u32,
    /// Joints in parent-before-child order.
    pub joints: Vec<JointStatic>,
}

/// `pose`: per-joint TRS (layout 0, 10 floats per joint) or scalar channels (layout 1).
#[derive(Debug, Clone, PartialEq)]
pub struct PoseFrame {
    /// Item handle.
    pub handle: u32,
    /// Frame info.
    pub info: FrameInfo,
    /// 0 = TRS, 1 = scalar.
    pub layout: u8,
    /// Joint (layout 0) or channel (layout 1) count.
    pub count: u16,
    /// `count * 10` floats for layout 0, `count` for layout 1.
    pub values: Vec<f32>,
}

/// One animation track of a pushed clip.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipTrack {
    /// Node or joint name.
    pub target: String,
    /// 0 translation, 1 rotation, 2 scale.
    pub property: u8,
    /// 0 step, 1 linear.
    pub interp: u8,
    /// Key times in seconds.
    pub times: Vec<f32>,
    /// 3 or 4 floats per key.
    pub values: Vec<f32>,
}

/// `clip_push`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipPush {
    /// Clip name.
    pub name: String,
    /// Frames per second.
    pub fps: f32,
    /// Duration in seconds.
    pub duration_s: f32,
    /// Whether the clip loops.
    pub looping: bool,
    /// Skeleton item handle the clip binds to, 0 for none.
    pub bind_handle: u32,
    /// Tracks.
    pub tracks: Vec<ClipTrack>,
}

/// Where a pushed texture's pixels come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureSource {
    /// A path inside the project or the inbox directory.
    Path(String),
    /// Encoded bytes: `container` 0 PNG, 1 JPEG, 2 EXR, 3 KTX2.
    Encoded {
        /// Container id.
        container: u8,
        /// The file bytes.
        bytes: Vec<u8>,
    },
    /// Raw pixels: `pixel_format` 0 rgba8, 1 rgba16f, 2 rgba32f.
    Raw {
        /// Pixel format id.
        pixel_format: u8,
        /// Width in texels.
        width: u32,
        /// Height in texels.
        height: u32,
        /// Pixel bytes.
        bytes: Vec<u8>,
    },
}

/// `texture_push`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TexturePush {
    /// Texture name.
    pub name: String,
    /// 0 srgb, 1 linear, 2 normal map.
    pub colorspace: u8,
    /// The source.
    pub source: TextureSource,
}

/// `ping` / `pong` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    /// Echoed token.
    pub token: u64,
    /// Sender clock.
    pub sender_time_ns: u64,
    /// Pong only: responder clock; 0 in a ping.
    pub responder_time_ns: u64,
}

/// `ack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    /// Highest seq applied.
    pub seq_applied: u32,
    /// Window bytes returned.
    pub credit_bytes: u32,
}

/// `session_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    /// State index of spec § 2.1.
    pub state: u8,
    /// Reason text.
    pub reason: String,
}

/// `resync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resync {
    /// Handle to re-send, 0 for every item.
    pub handle: u32,
}

/// The `error` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorFrame {
    /// Seq of the frame that caused it.
    pub ref_seq: u32,
    /// Item handle, 0 for none.
    pub handle: u32,
    /// Lowercase code.
    pub code: String,
    /// Detail.
    pub message: String,
    /// Whether the session closes.
    pub fatal: bool,
}

/// One decoded wire message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// `hello`
    Hello(Hello),
    /// `hello_ack`
    HelloAck(HelloAck),
    /// `item_declare`
    ItemDeclare(ItemDeclare),
    /// `item_delete`
    ItemDelete(u32),
    /// `transform`
    Transform(TransformFrame),
    /// `camera`
    Camera(CameraFrame),
    /// `light`
    Light(LightFrame),
    /// `mesh`
    Mesh(MeshUpdate),
    /// `skeleton_static`
    SkeletonStatic(SkeletonStatic),
    /// `pose`
    Pose(PoseFrame),
    /// `clip_push`
    ClipPush(ClipPush),
    /// `texture_push`
    TexturePush(TexturePush),
    /// `ping`
    Ping(Ping),
    /// `pong`
    Pong(Ping),
    /// `ack`
    Ack(Ack),
    /// `session_state`
    SessionState(SessionState),
    /// `resync`
    Resync(Resync),
    /// `error`
    Error(ErrorFrame),
    /// `goodbye` with its reason text.
    Goodbye(String),
    /// A message type this version does not know, flagged OPTIONAL by the sender.
    Unknown {
        /// The type id.
        msg_type: u16,
        /// Always true for a decoded value (a mandatory unknown type is an error).
        optional: bool,
    },
}

impl Message {
    /// The wire type id, or `None` for [`Message::Unknown`].
    #[must_use]
    pub fn msg_type(&self) -> Option<u16> {
        Some(match self {
            Self::Hello(_) => msg::HELLO,
            Self::HelloAck(_) => msg::HELLO_ACK,
            Self::ItemDeclare(_) => msg::ITEM_DECLARE,
            Self::ItemDelete(_) => msg::ITEM_DELETE,
            Self::Transform(_) => msg::TRANSFORM,
            Self::Camera(_) => msg::CAMERA,
            Self::Light(_) => msg::LIGHT,
            Self::Mesh(_) => msg::MESH,
            Self::SkeletonStatic(_) => msg::SKELETON_STATIC,
            Self::Pose(_) => msg::POSE,
            Self::ClipPush(_) => msg::CLIP_PUSH,
            Self::TexturePush(_) => msg::TEXTURE_PUSH,
            Self::Ping(_) => msg::PING,
            Self::Pong(_) => msg::PONG,
            Self::Ack(_) => msg::ACK,
            Self::SessionState(_) => msg::SESSION_STATE,
            Self::Resync(_) => msg::RESYNC,
            Self::Error(_) => msg::ERROR,
            Self::Goodbye(_) => msg::GOODBYE,
            Self::Unknown { .. } => return None,
        })
    }

    /// The item handle a data or declaration message addresses, if any.
    #[must_use]
    pub fn handle(&self) -> Option<u32> {
        match self {
            Self::ItemDeclare(m) => Some(m.handle),
            Self::ItemDelete(h) => Some(*h),
            Self::Transform(m) => Some(m.handle),
            Self::Camera(m) => Some(m.handle),
            Self::Light(m) => Some(m.handle),
            Self::Mesh(m) => Some(m.handle),
            Self::SkeletonStatic(m) => Some(m.handle),
            Self::Pose(m) => Some(m.handle),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

struct Writer<'a>(&'a mut Vec<u8>);

impl Writer<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32s(&mut self, v: &[f32]) {
        for x in v {
            self.f32(*x);
        }
    }
    fn str(&mut self, s: &str) -> Result<(), WireError> {
        let len = u16::try_from(s.len())
            .map_err(|_| WireError::new(ErrorCode::LimitExceeded, "string longer than 65535 bytes"))?;
        self.u16(len);
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
    fn info(&mut self, i: &FrameInfo) {
        self.u64(i.frame_id);
        self.u64(i.source_time_ns);
        match i.timecode {
            None => self.u8(0),
            Some(t) => {
                self.u8(1);
                self.u8(t.hh);
                self.u8(t.mm);
                self.u8(t.ss);
                self.u8(t.ff);
                self.u16(t.rate_num);
                self.u16(t.rate_den);
                self.u8(u8::from(t.drop_frame));
            }
        }
    }
    fn xf(&mut self, t: &TransformBlock) {
        self.f32s(&t.translation);
        self.f32s(&t.rotation);
        self.f32s(&t.scale);
        self.u8(u8::from(t.world_space));
    }
}

fn len_u32(n: usize, what: &str) -> Result<u32, WireError> {
    u32::try_from(n).map_err(|_| WireError::new(ErrorCode::LimitExceeded, format!("{what} too large")))
}

fn encode_payload(msg: &Message, out: &mut Vec<u8>) -> Result<(), WireError> {
    let mut w = Writer(out);
    match msg {
        Message::Hello(m) => {
            w.0.extend_from_slice(&HELLO_MAGIC);
            w.u32(m.min_version);
            w.u32(m.max_version);
            w.str(&m.client_name)?;
            w.str(&m.client_version)?;
            w.str(&m.dcc)?;
            w.str(&m.space)?;
            w.f32(m.unit_scale);
            w.u32(m.caps);
            w.str(&m.token)?;
        }
        Message::HelloAck(m) => {
            w.u32(m.version);
            w.u64(m.session_id);
            w.u8(u8::from(m.accepted));
            w.u32(m.caps);
            let l = &m.limits;
            w.u32(l.max_message_bytes);
            w.u32(l.max_vertices);
            w.u32(l.max_indices);
            w.u32(l.max_items);
            w.u32(l.max_joints);
            w.u32(l.max_clip_tracks);
            w.u32(l.max_keys_per_track);
            w.u32(l.max_texture_dim);
            w.u32(l.max_texture_bytes);
            w.u64(l.max_transient_texture_bytes);
            w.u32(l.max_key_len);
            w.u32(m.window_bytes);
            w.str(&m.server_name)?;
        }
        Message::ItemDeclare(m) => {
            w.u32(m.handle);
            w.str(&m.key)?;
            w.u8(m.kind as u8);
            w.u32(m.parent_handle);
            w.str(&m.display_name)?;
            w.u8(m.variant);
            if m.kind == FeedKind::Generic {
                let n = u16::try_from(m.channels.len())
                    .map_err(|_| WireError::new(ErrorCode::LimitExceeded, "too many channels"))?;
                w.u16(n);
                for c in &m.channels {
                    w.str(c)?;
                }
            }
        }
        Message::ItemDelete(h) => w.u32(*h),
        Message::Transform(m) => {
            w.u32(m.handle);
            w.info(&m.info);
            w.xf(&m.xf);
        }
        Message::Camera(m) => {
            w.u32(m.handle);
            w.info(&m.info);
            w.xf(&m.xf);
            match m.lens {
                Lens::Perspective {
                    yfov_rad,
                    aspect,
                    near,
                    far,
                } => {
                    w.u8(0);
                    w.f32s(&[yfov_rad, aspect, near, far]);
                }
                Lens::Orthographic {
                    xmag,
                    ymag,
                    near,
                    far,
                } => {
                    w.u8(1);
                    w.f32s(&[xmag, ymag, near, far]);
                }
            }
            let p = m.physical.unwrap_or(PhysicalCamera {
                focal_mm: 0.0,
                sensor_w_mm: 0.0,
                sensor_h_mm: 0.0,
                focus_m: 0.0,
                f_stop: 0.0,
            });
            w.f32s(&[p.focal_mm, p.sensor_w_mm, p.sensor_h_mm, p.focus_m, p.f_stop]);
        }
        Message::Light(m) => {
            w.u32(m.handle);
            w.info(&m.info);
            w.xf(&m.xf);
            w.f32s(&m.color);
            w.f32s(&[m.intensity, m.range, m.spot_inner_rad, m.spot_outer_rad]);
        }
        Message::Mesh(m) => {
            w.u32(m.handle);
            w.info(&m.info);
            w.u8(m.update_mode);
            let n = u16::try_from(m.sections.len())
                .map_err(|_| WireError::new(ErrorCode::LimitExceeded, "too many sections"))?;
            w.u16(n);
            for s in &m.sections {
                w.u8(s.section_id);
                w.u8(s.elem as u8);
                w.u16(s.index);
                w.u32(s.count);
                w.u32(len_u32(s.data.len(), "section")?);
                w.0.extend_from_slice(&s.data);
            }
        }
        Message::SkeletonStatic(m) => {
            w.u32(m.handle);
            let n = u16::try_from(m.joints.len())
                .map_err(|_| WireError::new(ErrorCode::LimitExceeded, "too many joints"))?;
            w.u16(n);
            for j in &m.joints {
                w.str(&j.name)?;
                w.i32(j.parent);
                w.f32s(&j.t);
                w.f32s(&j.r);
                w.f32s(&j.s);
            }
        }
        Message::Pose(m) => {
            w.u32(m.handle);
            w.info(&m.info);
            w.u8(m.layout);
            w.u16(m.count);
            w.f32s(&m.values);
        }
        Message::ClipPush(m) => {
            w.str(&m.name)?;
            w.f32(m.fps);
            w.f32(m.duration_s);
            w.u8(u8::from(m.looping));
            w.u32(m.bind_handle);
            w.u32(len_u32(m.tracks.len(), "tracks")?);
            for t in &m.tracks {
                w.str(&t.target)?;
                w.u8(t.property);
                w.u8(t.interp);
                w.u32(len_u32(t.times.len(), "keys")?);
                w.f32s(&t.times);
                w.f32s(&t.values);
            }
        }
        Message::TexturePush(m) => {
            w.str(&m.name)?;
            w.u8(m.colorspace);
            match &m.source {
                TextureSource::Path(p) => {
                    w.u8(0);
                    w.str(p)?;
                }
                TextureSource::Encoded { container, bytes } => {
                    w.u8(1);
                    w.u8(*container);
                    w.u32(len_u32(bytes.len(), "texture")?);
                    w.0.extend_from_slice(bytes);
                }
                TextureSource::Raw {
                    pixel_format,
                    width,
                    height,
                    bytes,
                } => {
                    w.u8(2);
                    w.u8(*pixel_format);
                    w.u32(*width);
                    w.u32(*height);
                    w.u32(len_u32(bytes.len(), "texture")?);
                    w.0.extend_from_slice(bytes);
                }
            }
        }
        Message::Ping(p) => {
            w.u64(p.token);
            w.u64(p.sender_time_ns);
        }
        Message::Pong(p) => {
            w.u64(p.token);
            w.u64(p.sender_time_ns);
            w.u64(p.responder_time_ns);
        }
        Message::Ack(a) => {
            w.u32(a.seq_applied);
            w.u32(a.credit_bytes);
        }
        Message::SessionState(s) => {
            w.u8(s.state);
            w.str(&s.reason)?;
        }
        Message::Resync(r) => w.u32(r.handle),
        Message::Error(e) => {
            w.u32(e.ref_seq);
            w.u32(e.handle);
            w.str(&e.code)?;
            w.str(&e.message)?;
            w.u8(u8::from(e.fatal));
        }
        Message::Goodbye(reason) => w.str(reason)?,
        Message::Unknown { .. } => {
            return Err(WireError::new(
                ErrorCode::Internal,
                "an unknown message cannot be encoded",
            ));
        }
    }
    Ok(())
}

/// Appends `msg` as one complete frame (header plus payload) to `out`.
///
/// # Errors
///
/// `limit_exceeded` when a string, count or the payload does not fit its length field, and
/// `internal` for [`Message::Unknown`].
pub fn encode(msg: &Message, seq: u32, out: &mut Vec<u8>) -> Result<(), WireError> {
    let start = out.len();
    out.extend_from_slice(&[0u8; HEADER_BYTES]);
    let result = encode_payload(msg, out);
    let msg_type = msg.msg_type().unwrap_or(0);
    finish_frame(out, start, msg_type, 0, seq, result)
}

fn finish_frame(
    out: &mut Vec<u8>,
    start: usize,
    msg_type: u16,
    flags: u16,
    seq: u32,
    result: Result<(), WireError>,
) -> Result<(), WireError> {
    if let Err(e) = result {
        out.truncate(start);
        return Err(e);
    }
    let payload = out.len() - start - HEADER_BYTES;
    let Ok(len) = u32::try_from(payload + 8) else {
        out.truncate(start);
        return Err(WireError::new(
            ErrorCode::FrameTooLarge,
            "payload longer than 4 GiB",
        ));
    };
    let header = FrameHeader {
        len,
        msg_type,
        flags,
        seq,
    };
    out[start..start + HEADER_BYTES].copy_from_slice(&header.to_bytes());
    Ok(())
}

/// Appends `msg` as consecutive fragments of at most `max_payload` payload bytes each, with
/// `FLAG_FRAGMENT_MORE` on all but the last. Sequence numbers run from `first_seq`; returns the
/// next unused one.
///
/// # Errors
///
/// As [`encode`]; `max_payload` of 0 is `limit_exceeded`.
pub fn encode_fragments(
    msg: &Message,
    first_seq: u32,
    max_payload: usize,
    out: &mut Vec<u8>,
) -> Result<u32, WireError> {
    if max_payload == 0 {
        return Err(WireError::new(ErrorCode::LimitExceeded, "fragment size 0"));
    }
    let mut payload = Vec::new();
    encode_payload(msg, &mut payload)?;
    let msg_type = msg.msg_type().unwrap_or(0);
    let mut seq = first_seq;
    let mut chunks = payload.chunks(max_payload).peekable();
    if chunks.peek().is_none() {
        encode(msg, seq, out)?;
        return Ok(seq.wrapping_add(1));
    }
    while let Some(chunk) = chunks.next() {
        let flags = if chunks.peek().is_some() {
            FLAG_FRAGMENT_MORE
        } else {
            0
        };
        let start = out.len();
        out.extend_from_slice(&[0u8; HEADER_BYTES]);
        out.extend_from_slice(chunk);
        finish_frame(out, start, msg_type, flags, seq, Ok(()))?;
        seq = seq.wrapping_add(1);
    }
    Ok(seq)
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Quaternions that were normalized because they were off unit length.
    renormalized: u32,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            renormalized: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if n > self.remaining() {
            return Err(malformed("payload truncated"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, WireError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// A finite float.
    fn f32(&mut self) -> Result<f32, WireError> {
        let b = self.take(4)?;
        let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(WireError::new(ErrorCode::InvalidValue, "non-finite float"))
        }
    }

    fn f32n<const N: usize>(&mut self) -> Result<[f32; N], WireError> {
        let mut out = [0.0f32; N];
        for o in &mut out {
            *o = self.f32()?;
        }
        Ok(out)
    }

    /// `n` finite floats; `n` is checked against the remaining bytes before allocating.
    fn f32_vec(&mut self, n: usize) -> Result<Vec<f32>, WireError> {
        let bytes = n.checked_mul(4).ok_or_else(|| malformed("count overflow"))?;
        let raw = self.take(bytes)?;
        let mut out = Vec::with_capacity(n);
        let (chunks, _) = raw.as_chunks::<4>();
        for c in chunks {
            let v = f32::from_le_bytes(*c);
            if !v.is_finite() {
                return Err(WireError::new(ErrorCode::InvalidValue, "non-finite float"));
            }
            out.push(v);
        }
        Ok(out)
    }

    fn string(&mut self) -> Result<String, WireError> {
        let n = usize::from(self.u16()?);
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| malformed("invalid UTF-8"))
    }

    fn quat(&mut self) -> Result<[f32; 4], WireError> {
        let mut q = self.f32n::<4>()?;
        let len2 = q.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>();
        let len = len2.sqrt();
        if len < 1e-12 {
            return Err(WireError::new(ErrorCode::InvalidValue, "zero-length quaternion"));
        }
        if (len - 1.0).abs() > 1e-3 {
            for x in &mut q {
                *x = (f64::from(*x) / len) as f32;
            }
            self.renormalized += 1;
        }
        Ok(q)
    }

    fn info(&mut self) -> Result<FrameInfo, WireError> {
        let frame_id = self.u64()?;
        let source_time_ns = self.u64()?;
        let timecode = match self.u8()? {
            0 => None,
            1 => Some(Timecode {
                hh: self.u8()?,
                mm: self.u8()?,
                ss: self.u8()?,
                ff: self.u8()?,
                rate_num: self.u16()?,
                rate_den: self.u16()?,
                drop_frame: self.u8()? != 0,
            }),
            _ => return Err(malformed("bad has_timecode")),
        };
        Ok(FrameInfo {
            frame_id,
            source_time_ns,
            timecode,
        })
    }

    fn xf(&mut self) -> Result<TransformBlock, WireError> {
        Ok(TransformBlock {
            translation: self.f32n::<3>()?,
            rotation: self.quat()?,
            scale: self.f32n::<3>()?,
            world_space: self.u8()? & 1 != 0,
        })
    }

    fn finish(&self) {
        // Trailing bytes are allowed (forward compatibility, spec § 4.3): nothing to check.
    }
}

fn check_count(n: u32, max: u32, what: &str) -> Result<usize, WireError> {
    if n > max {
        return Err(WireError::new(
            ErrorCode::LimitExceeded,
            format!("{what} count {n} above limit {max}"),
        ));
    }
    Ok(n as usize)
}

/// Decodes one complete (reassembled) message. Returns the message and how many quaternions were
/// renormalized (a warning count, spec § 4.8).
///
/// # Errors
///
/// The documented [`ErrorCode`] for each failure; never panics.
pub fn decode_with_warnings(
    header: FrameHeader,
    payload: &[u8],
    limits: &Limits,
) -> Result<(Message, u32), WireError> {
    if header.flags & FLAG_COMPRESSED != 0 {
        return Err(malformed("COMPRESSED flag is reserved in v1"));
    }
    if payload.len() > limits.max_message_bytes as usize {
        return Err(WireError::new(
            ErrorCode::FrameTooLarge,
            "message above max_message_bytes",
        ));
    }
    let mut r = Reader::new(payload);
    let m = decode_inner(&mut r, header, limits)?;
    r.finish();
    Ok((m, r.renormalized))
}

/// [`decode_with_warnings`] without the warning count.
///
/// # Errors
///
/// As [`decode_with_warnings`].
pub fn decode(header: FrameHeader, payload: &[u8], limits: &Limits) -> Result<Message, WireError> {
    decode_with_warnings(header, payload, limits).map(|(m, _)| m)
}

fn decode_inner(r: &mut Reader<'_>, header: FrameHeader, limits: &Limits) -> Result<Message, WireError> {
    Ok(match header.msg_type {
        msg::HELLO => {
            if r.take(4)? != HELLO_MAGIC {
                return Err(WireError::new(ErrorCode::BadMagic, "hello magic is not RFDP"));
            }
            let min_version = r.u32()?;
            let max_version = r.u32()?;
            let client_name = r.string()?;
            let client_version = r.string()?;
            let dcc = r.string()?;
            let space = r.string()?;
            if space != "gltf" && space != "regia" {
                return Err(WireError::new(
                    ErrorCode::InvalidValue,
                    "space must be gltf or regia",
                ));
            }
            let unit_scale = r.f32()?;
            if unit_scale <= 0.0 {
                return Err(WireError::new(
                    ErrorCode::InvalidValue,
                    "unit_scale must be positive",
                ));
            }
            let caps = r.u32()?;
            let token = r.string()?;
            Message::Hello(Hello {
                min_version,
                max_version,
                client_name,
                client_version,
                dcc,
                space,
                unit_scale,
                caps,
                token,
            })
        }
        msg::HELLO_ACK => {
            let version = r.u32()?;
            let session_id = r.u64()?;
            let accepted = r.u8()? != 0;
            let caps = r.u32()?;
            let limits = Limits {
                max_message_bytes: r.u32()?,
                max_vertices: r.u32()?,
                max_indices: r.u32()?,
                max_items: r.u32()?,
                max_joints: r.u32()?,
                max_clip_tracks: r.u32()?,
                max_keys_per_track: r.u32()?,
                max_texture_dim: r.u32()?,
                max_texture_bytes: r.u32()?,
                max_transient_texture_bytes: r.u64()?,
                max_key_len: r.u32()?,
            };
            let window_bytes = r.u32()?;
            let server_name = r.string()?;
            Message::HelloAck(HelloAck {
                version,
                session_id,
                accepted,
                caps,
                limits,
                window_bytes,
                server_name,
            })
        }
        msg::ITEM_DECLARE => {
            let handle = r.u32()?;
            if handle == 0 {
                return Err(WireError::new(ErrorCode::InvalidValue, "handle 0 is reserved"));
            }
            let key = r.string()?;
            if key.is_empty() || key.len() > limits.max_key_len as usize {
                return Err(WireError::new(
                    ErrorCode::LimitExceeded,
                    "key empty or above max_key_len",
                ));
            }
            let kind = FeedKind::from_u8(r.u8()?)
                .ok_or_else(|| WireError::new(ErrorCode::KindNotSupported, "unknown item kind"))?;
            let parent_handle = r.u32()?;
            let display_name = r.string()?;
            let variant = r.u8()?;
            let mut channels = Vec::new();
            if kind == FeedKind::Generic {
                let n = usize::from(r.u16()?);
                // Each name costs at least its 2-byte length.
                if n * 2 > r.remaining() {
                    return Err(malformed("channel count exceeds payload"));
                }
                for _ in 0..n {
                    channels.push(r.string()?);
                }
            }
            Message::ItemDeclare(ItemDeclare {
                handle,
                key,
                kind,
                parent_handle,
                display_name,
                variant,
                channels,
            })
        }
        msg::ITEM_DELETE => Message::ItemDelete(r.u32()?),
        msg::TRANSFORM => {
            let handle = r.u32()?;
            let info = r.info()?;
            Message::Transform(TransformFrame {
                handle,
                info,
                xf: r.xf()?,
            })
        }
        msg::CAMERA => {
            let handle = r.u32()?;
            let info = r.info()?;
            let xf = r.xf()?;
            let lens = match r.u8()? {
                0 => {
                    let [yfov_rad, aspect, near, far] = r.f32n::<4>()?;
                    Lens::Perspective {
                        yfov_rad,
                        aspect,
                        near,
                        far,
                    }
                }
                1 => {
                    let [xmag, ymag, near, far] = r.f32n::<4>()?;
                    Lens::Orthographic {
                        xmag,
                        ymag,
                        near,
                        far,
                    }
                }
                _ => return Err(WireError::new(ErrorCode::InvalidValue, "unknown camera mode")),
            };
            let [focal_mm, sensor_w_mm, sensor_h_mm, focus_m, f_stop] = r.f32n::<5>()?;
            let physical = if [focal_mm, sensor_w_mm, sensor_h_mm, focus_m, f_stop]
                .iter()
                .all(|v| *v == 0.0)
            {
                None
            } else {
                Some(PhysicalCamera {
                    focal_mm,
                    sensor_w_mm,
                    sensor_h_mm,
                    focus_m,
                    f_stop,
                })
            };
            Message::Camera(CameraFrame {
                handle,
                info,
                xf,
                lens,
                physical,
            })
        }
        msg::LIGHT => {
            let handle = r.u32()?;
            let info = r.info()?;
            let xf = r.xf()?;
            let color = r.f32n::<3>()?;
            let [intensity, range, spot_inner_rad, spot_outer_rad] = r.f32n::<4>()?;
            Message::Light(LightFrame {
                handle,
                info,
                xf,
                color,
                intensity,
                range,
                spot_inner_rad,
                spot_outer_rad,
            })
        }
        msg::MESH => Message::Mesh(decode_mesh(r, limits)?),
        msg::SKELETON_STATIC => {
            let handle = r.u32()?;
            let n = usize::from(r.u16()?);
            if n > limits.max_joints as usize {
                return Err(WireError::new(
                    ErrorCode::LimitExceeded,
                    "joint count above limit",
                ));
            }
            // A joint is at least 2 (name length) + 4 + 40 bytes.
            if n * 46 > r.remaining() {
                return Err(malformed("joint count exceeds payload"));
            }
            let mut joints = Vec::with_capacity(n);
            for i in 0..n {
                let name = r.string()?;
                let parent = r.i32()?;
                if parent < -1 || i64::from(parent) >= i as i64 {
                    return Err(WireError::new(
                        ErrorCode::InvalidValue,
                        "joint parent must precede the joint",
                    ));
                }
                joints.push(JointStatic {
                    name,
                    parent,
                    t: r.f32n::<3>()?,
                    r: r.quat()?,
                    s: r.f32n::<3>()?,
                });
            }
            Message::SkeletonStatic(SkeletonStatic { handle, joints })
        }
        msg::POSE => {
            let handle = r.u32()?;
            let info = r.info()?;
            let layout = r.u8()?;
            let count = r.u16()?;
            let (per, max) = match layout {
                0 => (10usize, limits.max_joints),
                1 => (1usize, limits.max_joints),
                _ => return Err(WireError::new(ErrorCode::InvalidValue, "unknown pose layout")),
            };
            if u32::from(count) > max {
                return Err(WireError::new(ErrorCode::LimitExceeded, "pose count above limit"));
            }
            let values = r.f32_vec(usize::from(count) * per)?;
            if layout == 0 {
                // Rotations must be usable unit quaternions: same rule as the transform block.
                let mut values = values;
                for j in 0..usize::from(count) {
                    let q = &mut values[j * 10 + 3..j * 10 + 7];
                    let len = q
                        .iter()
                        .map(|x| f64::from(*x) * f64::from(*x))
                        .sum::<f64>()
                        .sqrt();
                    if len < 1e-12 {
                        return Err(WireError::new(ErrorCode::InvalidValue, "zero-length quaternion"));
                    }
                    if (len - 1.0).abs() > 1e-3 {
                        for x in q.iter_mut() {
                            *x = (f64::from(*x) / len) as f32;
                        }
                        r.renormalized += 1;
                    }
                }
                Message::Pose(PoseFrame {
                    handle,
                    info,
                    layout,
                    count,
                    values,
                })
            } else {
                Message::Pose(PoseFrame {
                    handle,
                    info,
                    layout,
                    count,
                    values,
                })
            }
        }
        msg::CLIP_PUSH => {
            let name = r.string()?;
            let fps = r.f32()?;
            let duration_s = r.f32()?;
            let looping = r.u8()? != 0;
            let bind_handle = r.u32()?;
            let track_count = check_count(r.u32()?, limits.max_clip_tracks, "track")?;
            // A track is at least 2 + 1 + 1 + 4 bytes.
            if track_count * 8 > r.remaining() {
                return Err(malformed("track count exceeds payload"));
            }
            let mut tracks = Vec::with_capacity(track_count);
            for _ in 0..track_count {
                let target = r.string()?;
                let property = r.u8()?;
                let interp = r.u8()?;
                if property > 2 || interp > 1 {
                    return Err(WireError::new(
                        ErrorCode::InvalidValue,
                        "unknown track property or interpolation",
                    ));
                }
                let keys = check_count(r.u32()?, limits.max_keys_per_track, "key")?;
                let times = r.f32_vec(keys)?;
                let width = if property == 1 { 4 } else { 3 };
                let values = r.f32_vec(keys * width)?;
                tracks.push(ClipTrack {
                    target,
                    property,
                    interp,
                    times,
                    values,
                });
            }
            Message::ClipPush(ClipPush {
                name,
                fps,
                duration_s,
                looping,
                bind_handle,
                tracks,
            })
        }
        msg::TEXTURE_PUSH => {
            let name = r.string()?;
            let colorspace = r.u8()?;
            if colorspace > 2 {
                return Err(WireError::new(ErrorCode::InvalidValue, "unknown colorspace"));
            }
            let source = match r.u8()? {
                0 => TextureSource::Path(checked_source_path(r.string()?)?),
                1 => {
                    let container = r.u8()?;
                    if container > 3 {
                        return Err(WireError::new(ErrorCode::InvalidValue, "unknown container"));
                    }
                    let n = r.u32()?;
                    if n > limits.max_texture_bytes {
                        return Err(WireError::new(
                            ErrorCode::LimitExceeded,
                            "texture above max_texture_bytes",
                        ));
                    }
                    TextureSource::Encoded {
                        container,
                        bytes: r.take(n as usize)?.to_vec(),
                    }
                }
                2 => {
                    let pixel_format = r.u8()?;
                    let width = r.u32()?;
                    let height = r.u32()?;
                    let n = r.u32()?;
                    let bpp: u64 = match pixel_format {
                        0 => 4,
                        1 => 8,
                        2 => 16,
                        _ => return Err(WireError::new(ErrorCode::InvalidValue, "unknown pixel format")),
                    };
                    if width == 0
                        || height == 0
                        || width > limits.max_texture_dim
                        || height > limits.max_texture_dim
                    {
                        return Err(WireError::new(
                            ErrorCode::LimitExceeded,
                            "texture dimension above max_texture_dim",
                        ));
                    }
                    if n > limits.max_texture_bytes {
                        return Err(WireError::new(
                            ErrorCode::LimitExceeded,
                            "texture above max_texture_bytes",
                        ));
                    }
                    // `Limits` are public and advertised, so the product can leave `u64`.
                    let expected = u64::from(width)
                        .checked_mul(u64::from(height))
                        .and_then(|pixels| pixels.checked_mul(bpp));
                    if expected != Some(u64::from(n)) {
                        return Err(malformed("raw texture byte length does not match dimensions"));
                    }
                    TextureSource::Raw {
                        pixel_format,
                        width,
                        height,
                        bytes: r.take(n as usize)?.to_vec(),
                    }
                }
                _ => return Err(WireError::new(ErrorCode::InvalidValue, "unknown texture source")),
            };
            Message::TexturePush(TexturePush {
                name,
                colorspace,
                source,
            })
        }
        msg::PING => Message::Ping(Ping {
            token: r.u64()?,
            sender_time_ns: r.u64()?,
            responder_time_ns: 0,
        }),
        msg::PONG => Message::Pong(Ping {
            token: r.u64()?,
            sender_time_ns: r.u64()?,
            responder_time_ns: r.u64()?,
        }),
        msg::ACK => Message::Ack(Ack {
            seq_applied: r.u32()?,
            credit_bytes: r.u32()?,
        }),
        msg::SESSION_STATE => Message::SessionState(SessionState {
            state: r.u8()?,
            reason: r.string()?,
        }),
        msg::RESYNC => Message::Resync(Resync { handle: r.u32()? }),
        msg::ERROR => Message::Error(ErrorFrame {
            ref_seq: r.u32()?,
            handle: r.u32()?,
            code: r.string()?,
            message: r.string()?,
            fatal: r.u8()? != 0,
        }),
        msg::GOODBYE => Message::Goodbye(r.string()?),
        other => {
            if header.flags & FLAG_OPTIONAL != 0 {
                Message::Unknown {
                    msg_type: other,
                    optional: true,
                }
            } else {
                return Err(WireError::new(
                    ErrorCode::UnsupportedMessage,
                    format!("unknown mandatory message type {other:#06x}"),
                ));
            }
        }
    })
}

fn decode_mesh(r: &mut Reader<'_>, limits: &Limits) -> Result<MeshUpdate, WireError> {
    let handle = r.u32()?;
    let info = r.info()?;
    let update_mode = r.u8()?;
    if update_mode > 1 {
        return Err(WireError::new(ErrorCode::InvalidValue, "unknown update_mode"));
    }
    let n = usize::from(r.u16()?);
    // A section header is 12 bytes.
    if n * 12 > r.remaining() {
        return Err(malformed("section count exceeds payload"));
    }
    let mut sections = Vec::with_capacity(n);
    for _ in 0..n {
        let section_id = r.u8()?;
        let elem = Elem::from_u8(r.u8()?).ok_or_else(|| malformed("unknown element type"))?;
        let index = r.u16()?;
        let count = r.u32()?;
        let byte_len = r.u32()? as usize;
        let data = r.take(byte_len)?.to_vec();
        let section = MeshSection {
            section_id,
            elem,
            index,
            count,
            data,
        };
        validate_section(&section, limits)?;
        sections.push(section);
    }
    let mesh = MeshUpdate {
        handle,
        info,
        update_mode,
        sections,
    };
    validate_mesh(&mesh, limits)?;
    Ok(mesh)
}

/// Bytes one item of a known section takes, or `None` when the element type is not allowed for
/// that section. Unknown sections return `Some(0)`: they are skipped, not checked.
fn section_item_bytes(section_id: u8, elem: Elem) -> Option<usize> {
    use section::*;
    Some(match (section_id, elem) {
        (POSITIONS | NORMALS, Elem::F32) => 12,
        (TANGENTS | WEIGHTS, Elem::F32) => 16,
        (UV, Elem::F32) => 8,
        (COLOR, Elem::F32) => 16,
        (COLOR, Elem::U8) => 4,
        (INDICES, Elem::U16) => 2,
        (INDICES, Elem::U32) => 4,
        (SUBMESHES, Elem::U32) => 12,
        (JOINTS, Elem::U16) => 8,
        (MATERIAL_SLOTS, _) => 0, // variable size, opaque here: the frame's length bounds it
        (POSITIONS..=WEIGHTS, _) => return None,
        _ => 0,
    })
}

fn validate_section(s: &MeshSection, limits: &Limits) -> Result<(), WireError> {
    let Some(item) = section_item_bytes(s.section_id, s.elem) else {
        return Err(malformed("element type not allowed for this section"));
    };
    if item == 0 {
        return Ok(());
    }
    let max = match s.section_id {
        section::INDICES => limits.max_indices,
        section::SUBMESHES => limits.max_indices / 3 + 1,
        _ => limits.max_vertices,
    };
    check_count(s.count, max, "section item")?;
    if s.data.len() != s.count as usize * item {
        return Err(malformed("section byte length does not match count"));
    }
    if s.section_id == section::UV && s.index > 3 {
        return Err(WireError::new(ErrorCode::InvalidValue, "uv set above 3"));
    }
    if s.section_id == section::COLOR && s.index != 0 {
        return Err(WireError::new(ErrorCode::InvalidValue, "only color set 0 exists"));
    }
    // Float payloads must be finite (spec § 4.8).
    if s.elem == Elem::F32 {
        let (chunks, _) = s.data.as_chunks::<4>();
        if chunks.iter().any(|c| !f32::from_le_bytes(*c).is_finite()) {
            return Err(WireError::new(
                ErrorCode::InvalidValue,
                "non-finite float in section",
            ));
        }
    }
    Ok(())
}

/// The shape rules of a `by path` source, the part of spec § 4.8 that needs no state: a relative
/// path passes [`regia_core::project_path::check_relative`], an absolute one only needs to be a
/// path, because it may name the project. [`resolve_source_path`] decides where it lands.
fn checked_source_path(path: String) -> Result<String, WireError> {
    let refused = if std::path::Path::new(&path).is_absolute() {
        path.contains('\0')
    } else {
        regia_core::project_path::check_relative(&path).is_err()
    };
    if refused {
        return Err(WireError::new(
            ErrorCode::PathDenied,
            "texture path is not a path the receiver may open",
        ));
    }
    Ok(path)
}

/// Where a `by path` source is opened from (spec § 4.8), for the apply step, which alone knows the
/// directories: after symlink resolution it must lie inside `project` or `inbox`
/// (`.regia/dcc_feed/inbox/`). [`decode`] only checks the path's shape, so nothing opens a
/// [`TextureSource::Path`] without this.
///
/// # Errors
/// `path_denied` for anything else, including a path whose directories cannot be resolved.
pub fn resolve_source_path(
    path: &str,
    project: &std::path::Path,
    inbox: &std::path::Path,
) -> Result<std::path::PathBuf, WireError> {
    regia_core::project_path::resolve_in_roots(&[project.to_path_buf(), inbox.to_path_buf()], path)
        .map_err(|e| WireError::new(ErrorCode::PathDenied, format!("texture path: {e}")))
}

/// Checks a vertex-only update against the vertex count of the mesh it modifies: every per-vertex
/// section it carries (positions, normals, tangents, uv, color, joints, weights) must hold exactly
/// `vertex_count` items (dcc audit H2).
///
/// # Errors
/// `topology_mismatch` when a section's count differs, or when the update is not vertex-only.
pub fn check_vertex_only(m: &MeshUpdate, vertex_count: u32) -> Result<(), WireError> {
    if m.update_mode != 1 {
        return Err(WireError::new(
            ErrorCode::TopologyMismatch,
            "not a vertex-only update",
        ));
    }
    let per_vertex = |id: u8| {
        matches!(
            id,
            section::POSITIONS
                | section::NORMALS
                | section::TANGENTS
                | section::UV
                | section::COLOR
                | section::JOINTS
                | section::WEIGHTS
        )
    };
    match m
        .sections
        .iter()
        .find(|s| per_vertex(s.section_id) && s.count != vertex_count)
    {
        Some(s) => Err(WireError::new(
            ErrorCode::TopologyMismatch,
            format!(
                "vertex-only section {} holds {} items, the mesh has {vertex_count} vertices",
                s.section_id, s.count
            ),
        )),
        None => Ok(()),
    }
}

fn validate_mesh(m: &MeshUpdate, limits: &Limits) -> Result<(), WireError> {
    // A repeated (section_id, index) pair would let a later section override the one that was
    // validated below (`find` sees only the first), so it is refused.
    for (i, s) in m.sections.iter().enumerate() {
        if m.sections[..i]
            .iter()
            .any(|o| o.section_id == s.section_id && o.index == s.index)
        {
            return Err(malformed("repeated mesh section"));
        }
    }
    let find = |id: u8| m.sections.iter().find(|s| s.section_id == id);
    let positions = find(section::POSITIONS);
    let indices = find(section::INDICES);
    if m.update_mode == 0 && (positions.is_none() || indices.is_none()) {
        return Err(malformed("a full mesh update needs positions and indices"));
    }
    let vertex_count = positions.map(|p| p.count);
    // Every per-vertex section must match the position count when positions are present.
    if let Some(vc) = vertex_count {
        for s in &m.sections {
            let per_vertex = matches!(
                s.section_id,
                section::NORMALS
                    | section::TANGENTS
                    | section::UV
                    | section::COLOR
                    | section::JOINTS
                    | section::WEIGHTS
            );
            if per_vertex && s.count != vc {
                return Err(malformed("per-vertex section count differs from positions"));
            }
        }
    }
    if let Some(ix) = indices {
        if m.update_mode == 1 {
            return Err(malformed("a vertex-only update cannot carry indices"));
        }
        if ix.count % 3 != 0 {
            return Err(malformed("index count is not a multiple of 3"));
        }
        let vc = vertex_count.unwrap_or(0);
        let out_of_range = match ix.elem {
            Elem::U16 => ix
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .any(|c| u32::from(u16::from_le_bytes(*c)) >= vc),
            _ => ix
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|c| u32::from_le_bytes(*c) >= vc),
        };
        if out_of_range {
            return Err(WireError::new(ErrorCode::InvalidValue, "index out of range"));
        }
        if let Some(sub) = find(section::SUBMESHES) {
            let mut expected_slot = 0u32;
            let mut end = 0u32;
            for c in sub.data.as_chunks::<12>().0 {
                let slot = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                let first = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
                let n = u32::from_le_bytes([c[8], c[9], c[10], c[11]]);
                let range_end = first.checked_add(n);
                if slot < expected_slot || first < end || range_end.is_none_or(|e| e > ix.count) {
                    return Err(WireError::new(
                        ErrorCode::InvalidValue,
                        "submesh ranges must be ordered, disjoint and inside the index list",
                    ));
                }
                expected_slot = slot;
                end = range_end.unwrap_or(end);
            }
        }
    }
    let _ = limits;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reading frames from a stream
// ---------------------------------------------------------------------------------------------

/// Reads whole frames from a byte stream, bounded: the declared length is checked against the
/// limit before any payload buffer is allocated.
pub struct FrameReader<R: Read> {
    inner: R,
}

impl<R: Read> FrameReader<R> {
    /// Wraps `inner`.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// The next frame, or `Ok(None)` at a clean end of stream (EOF before the first header
    /// byte).
    ///
    /// # Errors
    ///
    /// `frame_too_large` when the declared length is above `max_message_bytes`,
    /// `malformed_frame` for a length below the 8 header bytes or a stream that ends inside a
    /// frame, `timeout` when the stream's read deadline passes, `internal` for any other I/O error.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self, limits: &Limits) -> Result<Option<(FrameHeader, Vec<u8>)>, WireError> {
        let mut head = [0u8; HEADER_BYTES];
        let mut got = 0;
        while got < HEADER_BYTES {
            match self.inner.read(&mut head[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(malformed("stream ended inside a frame header")),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(read_failed(&e)),
            }
        }
        let header = FrameHeader::parse(&head);
        let Some(payload_len) = header.payload_len() else {
            return Err(malformed("frame length below the 8 header bytes"));
        };
        if payload_len > limits.max_message_bytes as usize {
            return Err(WireError::new(
                ErrorCode::FrameTooLarge,
                "frame above max_message_bytes",
            ));
        }
        // Grows with what arrives: a declared length is a claim, and a peer that stalls after a
        // header must not pin `max_message_bytes` of zeroed memory.
        let mut payload = Vec::new();
        (&mut self.inner)
            .take(payload_len as u64)
            .read_to_end(&mut payload)
            .map_err(|e| read_failed(&e))?;
        if payload.len() != payload_len {
            return Err(malformed("stream ended inside a frame"));
        }
        Ok(Some((header, payload)))
    }
}

/// `timeout` for a socket's read deadline (the transport sets it; this reader has no clock).
fn read_failed(e: &std::io::Error) -> WireError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => WireError::new(
            ErrorCode::Timeout,
            "the peer sent nothing before the read deadline",
        ),
        _ => WireError::new(ErrorCode::Internal, e.to_string()),
    }
}

/// Reassembles fragmented messages (spec § 4.3).
#[derive(Debug, Default)]
pub struct Reassembler {
    pending: Option<(u16, u16, Vec<u8>)>,
    seq: u32,
}

/// A message ready to decode: the header carries the first fragment's `seq` and type, `flags`
/// as sent without the fragment bit, and `len` of the whole payload.
pub type Complete = (FrameHeader, Vec<u8>);

impl Reassembler {
    /// A reassembler with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one frame. Returns the complete message when this frame finishes one.
    ///
    /// `ping`, `pong` and `ack` may interleave a fragmented message and are returned at once.
    ///
    /// # Errors
    ///
    /// `malformed_frame` for a different message type inside a fragment run, `frame_too_large`
    /// when the accumulated size passes `max_message_bytes`.
    pub fn push(
        &mut self,
        header: FrameHeader,
        payload: Vec<u8>,
        limits: &Limits,
    ) -> Result<Option<Complete>, WireError> {
        let interleavable = matches!(header.msg_type, msg::PING | msg::PONG | msg::ACK);
        if interleavable && self.pending.is_some() {
            return Ok(Some((header, payload)));
        }
        let more = header.flags & FLAG_FRAGMENT_MORE != 0;
        match self.pending.take() {
            None if !more => Ok(Some((header, payload))),
            None => {
                if payload.len() > limits.max_message_bytes as usize {
                    return Err(WireError::new(
                        ErrorCode::FrameTooLarge,
                        "message above max_message_bytes",
                    ));
                }
                self.pending = Some((header.msg_type, header.flags & !FLAG_FRAGMENT_MORE, payload));
                self.seq = header.seq;
                Ok(None)
            }
            Some((msg_type, flags, mut buf)) => {
                if msg_type != header.msg_type {
                    return Err(malformed(
                        "a different message type interrupted a fragmented message",
                    ));
                }
                if buf.len() + payload.len() > limits.max_message_bytes as usize {
                    return Err(WireError::new(
                        ErrorCode::FrameTooLarge,
                        "message above max_message_bytes",
                    ));
                }
                buf.extend_from_slice(&payload);
                if more {
                    self.pending = Some((msg_type, flags, buf));
                    return Ok(None);
                }
                let len = u32::try_from(buf.len() + 8).unwrap_or(u32::MAX);
                Ok(Some((
                    FrameHeader {
                        len,
                        msg_type,
                        flags,
                        seq: self.seq,
                    },
                    buf,
                )))
            }
        }
    }
}
