// SPDX-License-Identifier: MPL-2.0
//! OpenXR head-mounted displays for Regia Engine (`docs/specs/58-openxr.md`, ADR-1676).
//!
//! This crate sits on the built seam `regia_platform::xr` (ADR-1571: the backend trait, the session
//! core, poses, projections, the late latch and the scripted `NullXr` backend) and adds the
//! engine-side parts that do not need a headset:
//!
//! - [`config`]: `config/xr.ron` ([`XrConfig`], `format_version` 1).
//! - [`state`]: the app-facing state ([`XrAppState`], [`XrState`]) and the stats counters.
//! - [`space`]: tracking spaces with their fallback order, recentering and the eye-height floor.
//! - [`input`]: the action manifest (`assets/xr/actions.xr.ron`), the attach-once freeze rule, the
//!   per-sync action states, the mapping onto `regia_input` and haptics.
//! - [`hands`]: hand-tracking joint snapshots ([`XrHands`]).
//! - [`comfort`]: snap turn, the motion vignette, `rig_turn` and `rig_move`.
//! - [`sim`]: the simulator script (`*.xrsim.ron`), its deterministic player and the pose recorder.
//! - [`mailbox`]: the single-slot frame mailbox between the XR frame loop and extraction.
//! - [`eye_targets`]: where eye images are rendered ([`EyeTargets`]); [`SimEyeTargets`] are
//!   ordinary wgpu textures.
//! - [`plugin`]: [`XrPlugin`], the ECS components (`XrRig`, `XrHead`, `XrController`, `XrEye`),
//!   messages and system sets.
//! - [`stereo`]: the stereo strategy, the shared cull frustum and the XR quality profile.
//! - [`render`]: [`XrRenderPlugin`]: the eyes, the comfort vignette, the hidden-area mask, quad and
//!   depth layers, the multiview strategy and the desktop mirror.
//! - [`budget`]: the frame budget: the `xr_over_budget` diagnostic ([`XrOverBudget`]) and
//!   `auto_scale` ([`XrFrameBudget`], spec 58 § 10).
//! - [`walk`]: [`XrWalkPlugin`]: smooth and teleport locomotion, ground follow, height calibration,
//!   comfort presets and the walk's render budget (ADR-2457).
//!
//! The real OpenXR binding (loader, instance, swapchains, the Vulkan device hand-off) is not in
//! this crate yet: spec 58 § 3.4 gates it on a feasibility spike that needs an XR runtime. The
//! [`XrBackendKind::OpenXr`] choice therefore resolves to the `Off` app state with the reason
//! `xr_no_loader` (see `book/agents/xr.md`).

pub mod budget;
pub mod comfort;
pub mod config;
pub mod eye_targets;
// Schema fingerprints for `cargo xtask check-formats` (ADR-2316).
pub mod formats;
pub mod hands;
pub mod input;
pub mod mailbox;
/// Frame counters the performance tests read.
pub mod perf;
pub mod plugin;
pub mod render;
pub mod sim;
pub mod space;
pub mod state;
pub mod stereo;
pub mod walk;

pub use budget::{XrFrameBudget, XrOverBudget};
pub use comfort::{ComfortSettings, DominantHand, SnapTurn, Vignette, rig_move, rig_turn};
pub use config::{Foveation, Mirror, XR_CONFIG_FORMAT_VERSION, XR_CONFIG_PATH, XrBackendKind, XrConfig};
pub use eye_targets::{EyeTargets, SimEyeTargets, box_resize};
pub use hands::{HAND_JOINT_COUNT, HandJoint, HandTrackingSupport, JointLocation, XrHands};
pub use input::{
    ActionDecl, ActionPoses, ActionSetDecl, ActionType, BindingDecl, HapticOutcome, MANIFEST_FORMAT_VERSION,
    MANIFEST_PATH, ProfileBindings, XrActionManifest, XrActionSets, XrActionStates, XrActionValue,
};
pub use mailbox::{XrFrameMailbox, XrFrameState};
pub use plugin::{
    AppliedAt, OPTION_KEYS, OptionValue, XrController, XrEye, XrFocusChanged, XrHead,
    XrInteractionProfileChanged, XrOriginChanged, XrPlugin, XrRefreshRateChanged, XrRig, XrRuntimeLost,
    XrSession, XrSet, XrStateChanged, drive_input, drive_pose, drive_session_state, eye_matrices,
    eye_projection, last_views, option_value, recenter, set_option,
};
pub use render::{XrPanel, XrPanels, XrRenderPlugin, XrRenderState, XrRenderStats};
pub use sim::{
    SIM_FORMAT_VERSION, ScriptPose, ScriptState, ScriptValue, SimPlayer, SimRecorder, SimTick, XrSimScript,
};
pub use space::{PlayArea, TrackingSpace, recenter_offset, resolve_tracking_space};
pub use state::{XrAppState, XrRenderProfile, XrState, XrStats, app_state_for};
pub use stereo::{StereoMode, XR_DISABLED_EFFECTS, apply_xr_quality, resolve_stereo_mode, union_cull_matrix};
pub use walk::{
    ComfortPreset, Locomotion, WalkBudget, WalkSettings, WalkStart, XrGround, XrWalk, XrWalkPlugin,
    apply_budget, apply_preset, calibrate_height, walk_budget, walk_start,
};
