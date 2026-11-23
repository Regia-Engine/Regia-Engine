// SPDX-License-Identifier: MPL-2.0
//! Runtime rebinding (`docs/specs/11-platform-and-input.md` § 7): the [`InputRebinder`] that
//! listens for the next raw input and turns it into a new binding, the [`InputOverrides`] it
//! writes (serialised to `user://input_overrides.ron`), conflict detection, and the `PreUpdate`
//! system that ties it together.
//!
//! Overrides are **merged over the project config** at startup ([`InputConfig::merge_overrides`])
//! and saved with `Vfs::write_atomic` immediately after every change, so a rebind survives a
//! restart (and an Android suspend/kill). Rebinding replaces *all* bindings of the action in the
//! target context with the single new one — classic "press a new key for Jump" semantics. A
//! capture in a device slot ([`RebindSlot`], ADR-2129) replaces only that device's bindings, so
//! rebinding a key keeps the gamepad binding, and [`AxisTuning`] adds a dead zone and invert to
//! the bindings of an axis. Named copies of the overrides live in `user://input_profiles/`.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_platform::{Vfs, VfsPath};

use crate::config::{
    ActionKind, Binding, DeadZone, DeviceClass, InputConfig, InputSource, Modifier, Swizzle, Trigger,
};
use crate::device::{Keyboard, Mouse};
use crate::gamepad::Gamepads;

/// Damaged copies of `input_overrides.ron` kept by [`InputOverrides::load_or_recover`]
/// (`input_overrides.ron.bad-1` .. `.bad-5`, ADR-3102).
pub const MAX_BAD_OVERRIDE_COPIES: u32 = 5;

/// `<path>.bad-<n>`.
fn bad_copy_path(path: &std::path::Path, n: u32) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".bad-{n}"));
    std::path::PathBuf::from(name)
}

/// Renames `path` to its next free `<name>.bad-<n>`; when all [`MAX_BAD_OVERRIDE_COPIES`] exist,
/// the oldest is dropped and the rest shift down (the rule of `regia_framework::user_file`,
/// ADR-3102; this crate cannot depend on the framework).
fn move_aside(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let free = (1..=MAX_BAD_OVERRIDE_COPIES)
        .map(|n| bad_copy_path(path, n))
        .find(|candidate| std::fs::symlink_metadata(candidate).is_err());
    let target = match free {
        Some(target) => target,
        None => {
            std::fs::remove_file(bad_copy_path(path, 1))?;
            for n in 2..=MAX_BAD_OVERRIDE_COPIES {
                std::fs::rename(bad_copy_path(path, n), bad_copy_path(path, n - 1))?;
            }
            bad_copy_path(path, MAX_BAD_OVERRIDE_COPIES)
        }
    };
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Where overrides are persisted (`user://` mount).
pub const OVERRIDES_PATH: &str = "user://input_overrides.ron";

/// Where named input profiles are kept: `user://input_profiles/<name>.ron` ([`save_profile`]).
pub const PROFILES_DIR: &str = "user://input_profiles";

/// The current `format_version` of `input_overrides.ron`. Version 1 had no version field and no
/// `slots`; it is a subset of version 2, so [`InputOverrides::from_ron`] reads it as it stands and
/// the next save writes version 2.
pub const OVERRIDES_VERSION: u32 = 2;

/// The `format_version` of a file that has none.
fn overrides_v1() -> u32 {
    1
}

/// The serde shape of `input_overrides.ron` and of the named input profiles
/// (`user://input_profiles/<name>.ron`, the same struct) for `cargo xtask check-formats`
/// (`docs/formats/input-overrides-ron.md`, ADR-2316): the field names of [`InputOverrides`] and
/// [`Binding`] and the variant names and payload shapes of every enum a binding reaches.
#[must_use]
pub fn input_overrides_schema_fingerprint() -> String {
    use crate::config::{DeadZoneMode, ResponseCurve, Stick};
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    fn variants<T: serde::Deserialize<'static>>() -> Vec<String> {
        let mut list = serde_variant_names::<T>();
        list.extend(serde_variant_shapes::<T>());
        list
    }
    join_field_lists(&[
        serde_field_names::<InputOverrides>(),
        variants::<RebindSlot>(),
        serde_field_names::<Binding>(),
        variants::<InputSource>(),
        variants::<regia_platform::MouseButton>(),
        variants::<regia_platform::GamepadButton>(),
        variants::<regia_platform::GamepadAxis>(),
        variants::<Stick>(),
        variants::<Modifier>(),
        serde_field_names::<DeadZone>(),
        variants::<DeadZoneMode>(),
        variants::<Swizzle>(),
        variants::<ResponseCurve>(),
        variants::<Trigger>(),
    ])
}

/// The `from` versions of the `input_overrides.ron` migration steps: v1 (no `format_version`, no
/// `slots`) is a subset of v2 and reads as it stands ([`InputOverrides::from_ron`]).
#[must_use]
pub fn input_overrides_migration_sources() -> Vec<u32> {
    vec![1]
}

/// `context → action → slot → replacement bindings`, the shape of [`InputOverrides::slots`].
pub type SlotOverrides = BTreeMap<String, BTreeMap<String, BTreeMap<RebindSlot, Vec<Binding>>>>;

/// User-rebound bindings: per context, per action, the replacement binding. Serialised to RON.
#[derive(Resource, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputOverrides {
    /// The file format version ([`OVERRIDES_VERSION`]).
    #[serde(default = "overrides_v1")]
    pub format_version: u32,
    /// `context → action → replacement binding`: replaces every binding of the action.
    #[serde(default)]
    pub contexts: BTreeMap<String, BTreeMap<String, Binding>>,
    /// Per-device overrides, merged after [`Self::contexts`]: each replaces only the bindings of
    /// the action on that slot's devices. Omitted from the file while empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: SlotOverrides,
}

impl Default for InputOverrides {
    fn default() -> Self {
        Self {
            format_version: OVERRIDES_VERSION,
            contexts: BTreeMap::new(),
            slots: BTreeMap::new(),
        }
    }
}

impl InputOverrides {
    /// Sets (or replaces) the override for one action in one context.
    pub fn set(&mut self, context: impl Into<String>, action: impl Into<String>, binding: Binding) {
        self.contexts
            .entry(context.into())
            .or_default()
            .insert(action.into(), binding);
    }

    /// Sets (or replaces) what one action binds on the devices of `slot`.
    /// [`RebindSlot::All`] is not a device slot: use [`Self::set`].
    pub fn set_slot(
        &mut self,
        context: impl Into<String>,
        action: impl Into<String>,
        slot: RebindSlot,
        bindings: Vec<Binding>,
    ) {
        self.slots
            .entry(context.into())
            .or_default()
            .entry(action.into())
            .or_default()
            .insert(slot, bindings);
    }

    /// Removes every override of one action (whole-action and per-device); true when there was
    /// one.
    pub fn clear(&mut self, context: &str, action: &str) -> bool {
        let whole = self
            .contexts
            .get_mut(context)
            .is_some_and(|actions| actions.remove(action).is_some());
        let mut slotted = false;
        if let Some(actions) = self.slots.get_mut(context) {
            slotted = actions.remove(action).is_some();
            if actions.is_empty() {
                self.slots.remove(context);
            }
        }
        whole || slotted
    }

    /// Serialises to RON, returning the serialisation error.
    ///
    /// [`InputOverrides::to_ron`] swallows a serialisation failure and returns an empty string;
    /// this variant surfaces it. Prefer this when the overrides must round-trip losslessly.
    pub fn try_to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(
            self,
            regia_core::ron_pretty().new_line(std::borrow::Cow::Borrowed("\n")),
        )
    }

    /// Serialises to RON. A serialisation failure returns an empty string; use
    /// [`InputOverrides::try_to_ron`] to surface the error instead.
    pub fn to_ron(&self) -> String {
        self.try_to_ron().unwrap_or_default()
    }

    /// Parses from RON. A version 1 file (no `format_version`) is read as it stands and comes back
    /// at [`OVERRIDES_VERSION`]; a file from a newer build is refused, so it is not overwritten.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        let mut overrides: Self = ron::from_str(text).map_err(|e| format!("input overrides RON: {e}"))?;
        if overrides.format_version > OVERRIDES_VERSION {
            return Err(format!(
                "input overrides format_version {} is newer than this build reads ({OVERRIDES_VERSION})",
                overrides.format_version
            ));
        }
        overrides.format_version = OVERRIDES_VERSION;
        Ok(overrides)
    }

    /// Loads overrides from `path`, if it exists; a missing file is an empty set, anything else
    /// is an error (corrupt files are reported, not silently ignored).
    pub fn load(vfs: &Vfs, path: &VfsPath) -> Result<Self, String> {
        if !vfs.exists(path) {
            return Ok(Self::default());
        }
        // `VfsError` already names the path; do not prefix it a second time.
        let bytes = vfs.read_sync(path).map_err(|e| e.to_string())?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            format!(
                "{path}: invalid UTF-8 at byte {} (the file is not UTF-8 text)",
                e.valid_up_to()
            )
        })?;
        Self::from_ron(text).map_err(|e| format!("{path}: {e}"))
    }

    /// The start-up read of `user://input_overrides.ron` (ADR-3102): [`InputOverrides::load`],
    /// except that a file that does not parse (not UTF-8, not RON, empty) or has a newer
    /// `format_version` is renamed to `input_overrides.ron.bad-<n>` (at most
    /// [`MAX_BAD_OVERRIDE_COPIES`], the oldest dropped first), one warn line names it, and the
    /// project bindings load (an empty set). The next rebind saves normally.
    ///
    /// # Errors
    /// A read failure, or a damaged file that cannot be moved aside (a read-only directory, a
    /// mount with no host path): the file is left as it is and the caller must not save over it.
    pub fn load_or_recover(vfs: &Vfs, path: &VfsPath) -> Result<Self, String> {
        if !vfs.exists(path) {
            return Ok(Self::default());
        }
        let bytes = vfs.read_sync(path).map_err(|e| e.to_string())?;
        let why = match Self::parse_bytes(path, &bytes) {
            Ok(overrides) => return Ok(overrides),
            Err(why) => why,
        };
        let Some(host) = vfs.host_path(path) else {
            return Err(why);
        };
        match move_aside(&host) {
            Ok(moved) => {
                tracing::warn!(
                    target: "regia_input",
                    "input overrides file {} is unusable ({why}); moved it to {} and using the project bindings",
                    host.display(),
                    moved.display()
                );
                Ok(Self::default())
            }
            Err(err) => Err(format!("{why}; it cannot be moved aside: {err}")),
        }
    }

    /// Parses the bytes of an overrides file read from `path` (for error text).
    fn parse_bytes(path: &VfsPath, bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| {
            format!(
                "{path}: invalid UTF-8 at byte {} (the file is not UTF-8 text)",
                e.valid_up_to()
            )
        })?;
        Self::from_ron(text).map_err(|e| format!("{path}: {e}"))
    }

    /// Saves overrides atomically (temp file + rename; `user://` and `cache://` only).
    pub fn save(&self, vfs: &Vfs, path: &VfsPath) -> Result<(), String> {
        vfs.write_atomic(path, self.to_ron().as_bytes())
            .map_err(|e| format!("{path}: {e}"))
    }
}

/// The outcome of one completed rebind, for UI feedback and tests.
#[derive(Debug, Clone, PartialEq)]
pub struct RebindOutcome {
    /// The context the binding lives in.
    pub context: String,
    /// The action that was rebound.
    pub action: String,
    /// The new source.
    pub input: InputSource,
    /// Other actions in the same context that already use `input` (conflicts, still applied).
    pub conflicts: Vec<String>,
    /// The device slot the capture was limited to.
    pub slot: RebindSlot,
}

/// Listens for the next raw input and rebinds an action to it (`docs/specs/11` § 7).
///
/// Start with [`InputRebinder::start`]; [`apply_rebinding`] picks up the next key / mouse button /
/// gamepad button / analog stick deflection (past the dead zone), updates the [`InputOverrides`]
/// resource, persists them, and leaves the [`RebindOutcome`] in [`InputRebinder::outcome`] for
/// the game to show.
///
/// # Example
///
/// ```
/// use regia_input::InputRebinder;
///
/// let mut rebinder = InputRebinder::default();
/// assert!(rebinder.pending().is_none());
///
/// rebinder.start("Gameplay", "Jump");
/// assert_eq!(rebinder.pending(), Some(("Gameplay", "Jump")));
///
/// rebinder.cancel();
/// assert!(rebinder.pending().is_none());
/// ```
#[derive(Resource, Debug, Default)]
pub struct InputRebinder {
    pending: Option<(String, String)>,
    mode: RebindMode,
    slot: RebindSlot,
    cancel_inputs: Vec<InputSource>,
    /// Mouse travel since the capture began, in device units ([`MOUSE_CAPTURE_TRAVEL`]).
    mouse_travel: f32,
    /// The press a capture consumed, while it can still reach the UI as a click: through the frame
    /// it comes up (the UI writes and reads its click in `Update`, after this pass).
    guard: Option<InputSource>,
    /// Sources captured so far by a multi-step capture ([`RebindMode::Keys1D`], [`RebindMode::Keys2D`],
    /// [`RebindMode::Chord`]).
    captured: Vec<InputSource>,
    /// Analog axes already past the capture threshold when the capture began (or since), keyed
    /// by pad id: a stick or trigger held through `start` is not a new input until it returns
    /// below the threshold. `None` until the first capture pass after `start`.
    held_axes: Option<BTreeSet<(u64, regia_platform::GamepadAxis)>>,
    /// The completed rebind of the previous [`apply_rebinding`] pass, until the next start.
    pub outcome: Option<RebindOutcome>,
    /// Every binding the previous completed rebind wrote (a chord writes two: the modifier's
    /// helper action and the main action), until the next start.
    pub applied: Vec<Binding>,
    /// Why the most recent rebind was not written to disk (persistence blocked, no `Vfs`
    /// write path, or a failed save); `None` when it was saved. The rebind itself still applies
    /// for the session.
    pub persist_error: Option<String>,
    persist_blocked: bool,
}

impl InputRebinder {
    /// Stops [`apply_rebinding`] from writing the overrides file (set when the existing file
    /// failed to load, so a corrupt file is not replaced by a one-binding map).
    pub fn block_persist(&mut self) {
        self.persist_blocked = true;
    }

    /// Allows saving again, e.g. after the user chose to reset the overrides file.
    pub fn allow_persist(&mut self) {
        self.persist_blocked = false;
    }

    /// Whether saving is blocked by a failed load.
    pub fn persist_blocked(&self) -> bool {
        self.persist_blocked
    }

    /// Begins listening: the next raw input rebinds `action` in `context`
    /// ([`RebindMode::Single`]).
    pub fn start(&mut self, context: impl Into<String>, action: impl Into<String>) {
        self.start_mode(context, action, RebindMode::Single);
    }

    /// Begins listening in `mode`: a multi-step mode ([`RebindMode::Keys1D`], [`RebindMode::Keys2D`],
    /// [`RebindMode::Chord`]) collects several inputs, one per press, before it rebinds.
    pub fn start_mode(&mut self, context: impl Into<String>, action: impl Into<String>, mode: RebindMode) {
        self.start_slot(context, action, mode, RebindSlot::All);
    }

    /// Begins listening in `mode`, on the devices of `slot` only: inputs of other devices are
    /// ignored, and the capture replaces only the action's bindings on `slot`'s devices.
    pub fn start_slot(
        &mut self,
        context: impl Into<String>,
        action: impl Into<String>,
        mode: RebindMode,
        slot: RebindSlot,
    ) {
        self.pending = Some((context.into(), action.into()));
        self.mode = mode;
        self.slot = slot;
        self.cancel_inputs.clear();
        self.mouse_travel = 0.0;
        self.captured.clear();
        self.held_axes = None;
        self.outcome = None;
        self.applied.clear();
    }

    /// Cancels a pending rebind (anything captured so far by a multi-step capture is dropped).
    pub fn cancel(&mut self) {
        self.pending = None;
        self.captured.clear();
        self.held_axes = None;
    }

    /// The inputs that cancel this capture instead of being captured (a press edge of a key or a
    /// pad button). None by default; the `regia_ui` rebind panel sets `Escape` and the pad's
    /// `Start`, so a player with one device can always leave a capture. Cleared by the next start.
    pub fn set_cancel_inputs(&mut self, inputs: Vec<InputSource>) {
        self.cancel_inputs = inputs;
    }

    /// The device slot of the pending (or last started) capture.
    pub fn slot(&self) -> RebindSlot {
        self.slot
    }

    /// True while a press consumed by a capture can still reach the UI: a widget that starts
    /// captures ignores clicks then, or the release of the click that bound `Mouse Left` would
    /// start the next capture.
    pub fn click_guard(&self) -> bool {
        self.guard.is_some()
    }

    /// Whether a rebind capture is in progress, and what it will rebind.
    pub fn pending(&self) -> Option<(&str, &str)> {
        self.pending.as_ref().map(|(c, a)| (c.as_str(), a.as_str()))
    }

    /// The mode of the pending (or last started) capture.
    pub fn mode(&self) -> RebindMode {
        self.mode
    }

    /// Where a pending capture stands: which step it waits for. `None` when idle.
    pub fn progress(&self) -> Option<RebindProgress> {
        self.pending.as_ref()?;
        let steps = self.mode.steps();
        let step = self.captured.len().min(steps.len().saturating_sub(1));
        Some(RebindProgress {
            mode: self.mode,
            step,
            total: steps.len(),
            prompt: steps[step],
            captured: self.captured.iter().map(source_label).collect(),
        })
    }

    /// Feeds one captured source into the pending capture. `None` while more inputs are needed or
    /// when `input` does not fit the current step (it is ignored and the capture keeps waiting);
    /// `Some(sources)` once the capture is complete.
    fn accept(&mut self, input: InputSource) -> Option<Vec<InputSource>> {
        match self.mode {
            RebindMode::Single => return Some(vec![input]),
            RebindMode::Keys1D | RebindMode::Keys2D => {
                if !matches!(input, InputSource::Key(_)) {
                    return None;
                }
            }
            RebindMode::Chord => {
                if !matches!(
                    input,
                    InputSource::Key(_) | InputSource::MouseButton(_) | InputSource::GamepadButton(_)
                ) {
                    return None;
                }
            }
        }
        if self.captured.contains(&input) {
            return None;
        }
        self.captured.push(input);
        (self.captured.len() == self.mode.steps().len()).then(|| std::mem::take(&mut self.captured))
    }
}

/// How far the mouse has to travel during a capture to bind mouse motion. Only a two- or three-axis
/// action on the keyboard and mouse slot listens for it, and this far, so a hand resting on the
/// mouse does not bind it.
pub const MOUSE_CAPTURE_TRAVEL: f32 = 200.0;

/// Which devices a capture listens to and replaces (ADR-2129).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum RebindSlot {
    /// Every device; the capture replaces all of the action's bindings.
    #[default]
    All,
    /// Keyboard and mouse.
    KeyboardMouse,
    /// Gamepads.
    Gamepad,
}

impl RebindSlot {
    /// The lowercase protocol name (`all`, `keyboard_mouse`, `gamepad`).
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::KeyboardMouse => "keyboard_mouse",
            Self::Gamepad => "gamepad",
        }
    }

    /// Parses [`RebindSlot::name`].
    pub fn parse(name: &str) -> Option<Self> {
        [Self::All, Self::KeyboardMouse, Self::Gamepad]
            .into_iter()
            .find(|slot| slot.name() == name)
    }

    /// Whether `source` reads a device of this slot.
    pub fn holds(self, source: &InputSource) -> bool {
        match self {
            Self::All => true,
            Self::KeyboardMouse => source.class() != DeviceClass::Gamepad,
            Self::Gamepad => source.class() == DeviceClass::Gamepad,
        }
    }

    /// Whether `mode` can capture on this slot: a pad has no key group.
    pub fn fits(self, mode: RebindMode) -> bool {
        self != Self::Gamepad || matches!(mode, RebindMode::Single | RebindMode::Chord)
    }
}

/// What a rebind capture collects (`docs/specs/11` § 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RebindMode {
    /// The next single input (key, mouse button, pad button, stick or pad axis).
    #[default]
    Single,
    /// Two keys, negative then positive, for a one-axis action ([`InputSource::Keys2D`] with the
    /// vertical keys unset).
    Keys1D,
    /// Four keys, up, down, left, right, for a two-axis action ([`InputSource::Keys2D`]).
    Keys2D,
    /// A chord: a modifier input, then the main input held with it
    /// ([`Trigger::Chord`] on a helper action, see [`chord_helper`]).
    Chord,
}

impl RebindMode {
    /// The prompt of each step, in capture order.
    pub fn steps(self) -> &'static [&'static str] {
        match self {
            Self::Single => &["input"],
            Self::Keys1D => &["negative", "positive"],
            Self::Keys2D => &["up", "down", "left", "right"],
            Self::Chord => &["modifier", "key"],
        }
    }

    /// The lowercase protocol name (`single`, `keys1d`, `keys2d`, `chord`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Keys1D => "keys1d",
            Self::Keys2D => "keys2d",
            Self::Chord => "chord",
        }
    }

    /// Parses [`RebindMode::name`].
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Single, Self::Keys1D, Self::Keys2D, Self::Chord]
            .into_iter()
            .find(|mode| mode.name() == name)
    }

    /// Whether this mode makes sense for an action of `kind`: key groups for axes, chords for
    /// buttons, a single input for anything.
    pub fn fits(self, kind: ActionKind) -> bool {
        match self {
            Self::Single => true,
            Self::Keys1D => kind == ActionKind::Axis1D,
            Self::Keys2D => matches!(kind, ActionKind::Axis2D | ActionKind::Axis3D),
            Self::Chord => kind == ActionKind::Button,
        }
    }
}

/// Where a pending multi-step capture stands ([`InputRebinder::progress`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebindProgress {
    /// The capture mode.
    pub mode: RebindMode,
    /// Zero-based index of the step being waited for.
    pub step: usize,
    /// Number of steps in the mode.
    pub total: usize,
    /// The prompt of the current step (`up`, `modifier`, ...).
    pub prompt: &'static str,
    /// Display labels of the inputs captured so far.
    pub captured: Vec<String>,
}

/// The name of the helper action that carries a chord's modifier: `"<action>.chord"`.
pub fn chord_helper(action: &str) -> String {
    format!("{action}.chord")
}

/// The chord helper of `action` for a capture on `slot`: each device slot keeps its own chord, so
/// a keyboard chord and a pad chord of one action do not overwrite each other's modifier.
pub fn chord_helper_in(action: &str, slot: RebindSlot) -> String {
    match slot {
        RebindSlot::All => chord_helper(action),
        RebindSlot::KeyboardMouse => format!("{action}.keyboard_mouse.chord"),
        RebindSlot::Gamepad => format!("{action}.gamepad.chord"),
    }
}

/// Whether `action` is a chord helper made by [`chord_helper`] (lists hide them).
pub fn is_chord_helper(action: &str) -> bool {
    action.ends_with(".chord")
}

/// A short display label for one source: `W`, `Left Ctrl`, `Mouse Left`, `Left Stick`.
pub fn source_label(source: &InputSource) -> String {
    match source {
        InputSource::Key(key) => key_label(key.as_str()),
        InputSource::Keys2D {
            up,
            down,
            left,
            right,
        } => {
            let keys: Vec<String> = [up, down, left, right]
                .into_iter()
                .filter(|k| !k.as_str().is_empty())
                .map(|k| key_label(k.as_str()))
                .collect();
            keys.join("/")
        }
        InputSource::MouseDelta => "Mouse Move".to_string(),
        InputSource::MouseButton(button) => format!("Mouse {}", mouse_button_label(*button)),
        InputSource::GamepadButton(button) => split_camel(&format!("{button:?}")),
        InputSource::GamepadStick(crate::Stick::Left) => "Left Stick".to_string(),
        InputSource::GamepadStick(crate::Stick::Right) => "Right Stick".to_string(),
        InputSource::GamepadAxis(axis) => split_camel(&format!("{axis:?}")),
    }
}

fn mouse_button_label(button: regia_platform::MouseButton) -> String {
    use regia_platform::MouseButton as B;
    match button {
        B::Left => "Left".to_string(),
        B::Right => "Right".to_string(),
        B::Middle => "Middle".to_string(),
        B::Back => "Back".to_string(),
        B::Forward => "Forward".to_string(),
        B::Other(n) => format!("{n}"),
    }
}

/// `KeyW` → `W`, `Digit1` → `1`, `ControlLeft` → `Left Control`, `ArrowUp` → `Up`.
fn key_label(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("Key").filter(|r| r.chars().count() == 1) {
        return rest.to_string();
    }
    if let Some(rest) = name.strip_prefix("Digit").filter(|r| r.chars().count() == 1) {
        return rest.to_string();
    }
    if let Some(rest) = name.strip_prefix("Arrow") {
        return rest.to_string();
    }
    for side in ["Left", "Right"] {
        if let Some(base) = name.strip_suffix(side).filter(|b| !b.is_empty()) {
            return format!("{side} {}", split_camel(base));
        }
    }
    split_camel(name)
}

/// Inserts a space before each capital that follows a lower-case letter or digit.
fn split_camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut previous: Option<char> = None;
    for c in name.chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            out.push(' ');
        }
        out.push(c);
        previous = Some(c);
    }
    out
}

/// The display label of the binding(s) of `action` in `context`: every binding joined with
/// ` / `, a chord shown as `Modifier + Key`. `None` when the context or action has no binding.
pub fn binding_label(config: &InputConfig, context: &str, action: &str) -> Option<String> {
    binding_label_in(config, context, action, RebindSlot::All)
}

/// [`binding_label`] limited to the bindings on the devices of `slot`.
pub fn binding_label_in(
    config: &InputConfig,
    context: &str,
    action: &str,
    slot: RebindSlot,
) -> Option<String> {
    let ctx = config.contexts.get(context)?;
    let labels: Vec<String> = ctx
        .bindings
        .iter()
        .filter(|b| b.action == action && slot.holds(&b.input))
        .map(|b| {
            let main = source_label(&b.input);
            let chord = b.triggers.iter().find_map(|t| match t {
                Trigger::Chord(helper) => ctx
                    .bindings
                    .iter()
                    .find(|other| &other.action == helper)
                    .map(|other| source_label(&other.input)),
                _ => None,
            });
            match chord {
                Some(modifier) => format!("{modifier} + {main}"),
                None => main,
            }
        })
        .collect();
    (!labels.is_empty()).then(|| labels.join(" / "))
}

/// Detects the first candidate source of this frame, in a fixed device order: keys, then mouse
/// buttons, then gamepad buttons, then gamepad stick axes. Gamepad axes only count past the
/// threshold so resting stick drift cannot be captured. Buttons and keys count only on their
/// press edge, so an input that was already down when the capture began (the click on a
/// "Rebind" button) is not captured. Within one device the choice is sorted
/// (key name / button ordinal / pad id), so captures are deterministic.
fn capture_source(
    keyboard: &Keyboard,
    mouse: &Mouse,
    gamepads: &Gamepads,
    held: &BTreeSet<(u64, regia_platform::GamepadAxis)>,
) -> Option<InputSource> {
    let mut keys = keyboard
        .just_pressed
        .iter()
        .map(|k| k.as_str().to_string())
        .collect::<Vec<_>>();
    keys.sort();
    if let Some(key) = keys.first() {
        return Some(InputSource::Key(crate::KeyCode::new(key.clone())));
    }
    let mut buttons = mouse.just_pressed.iter().copied().collect::<Vec<_>>();
    buttons.sort_by_key(|b| button_order(*b));
    if let Some(&button) = buttons.first() {
        return Some(InputSource::MouseButton(button));
    }
    let mut pad_buttons = gamepads
        .pads()
        .flat_map(|(id, pad)| {
            pad.buttons
                .iter()
                .filter(|(_, b)| b.just_pressed)
                .map(move |(button, _)| (id, *button))
        })
        .collect::<Vec<_>>();
    pad_buttons.sort();
    if let Some(&(_, button)) = pad_buttons.first() {
        return Some(InputSource::GamepadButton(button));
    }
    let mut axes = gamepads
        .pads()
        .flat_map(|(id, pad)| {
            pad.axes
                .iter()
                .filter(|(_, v)| v.abs() > AXIS_CAPTURE_THRESHOLD)
                .map(move |(axis, _)| (id, *axis))
        })
        .filter(|key| !held.contains(key))
        .collect::<Vec<_>>();
    axes.sort();
    if let Some(&(_, axis)) = axes.first() {
        return Some(match axis {
            regia_platform::GamepadAxis::LeftStickX | regia_platform::GamepadAxis::LeftStickY => {
                InputSource::GamepadStick(crate::Stick::Left)
            }
            regia_platform::GamepadAxis::RightStickX | regia_platform::GamepadAxis::RightStickY => {
                InputSource::GamepadStick(crate::Stick::Right)
            }
            other => InputSource::GamepadAxis(other),
        });
    }
    None
}

/// Deflection past which an analog axis counts as a rebind capture.
const AXIS_CAPTURE_THRESHOLD: f32 = 0.6;

/// Every (pad, axis) currently deflected past [`AXIS_CAPTURE_THRESHOLD`].
fn deflected_axes(gamepads: &Gamepads) -> BTreeSet<(u64, regia_platform::GamepadAxis)> {
    gamepads
        .pads()
        .flat_map(|(id, pad)| {
            pad.axes
                .iter()
                .filter(|(_, v)| v.abs() > AXIS_CAPTURE_THRESHOLD)
                .map(move |(axis, _)| (id, *axis))
        })
        .collect()
}

/// A stable ordinal for mouse buttons, so captures are deterministic.
fn button_order(button: regia_platform::MouseButton) -> u32 {
    use regia_platform::MouseButton as B;
    match button {
        B::Left => 0,
        B::Right => 1,
        B::Middle => 2,
        B::Back => 3,
        B::Forward => 4,
        B::Other(n) => 1000 + u32::from(n),
    }
}

/// Whether two sources would fire on a shared physical input: equal sources, or key groups that
/// share a key.
fn sources_overlap(a: &InputSource, b: &InputSource) -> bool {
    fn keys(source: &InputSource) -> Vec<&str> {
        match source {
            InputSource::Key(key) => vec![key.as_str()],
            InputSource::Keys2D {
                up,
                down,
                left,
                right,
            } => [up, down, left, right]
                .into_iter()
                .map(crate::KeyCode::as_str)
                .filter(|k| !k.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }
    /// The pad axes a source reads.
    fn axes(source: &InputSource) -> Vec<regia_platform::GamepadAxis> {
        match source {
            InputSource::GamepadStick(stick) => vec![stick.x(), stick.y()],
            InputSource::GamepadAxis(axis) => vec![*axis],
            _ => Vec::new(),
        }
    }
    if a == b {
        return true;
    }
    let (ka, kb) = (keys(a), keys(b));
    if ka.iter().any(|k| kb.contains(k)) {
        return true;
    }
    let (aa, ab) = (axes(a), axes(b));
    aa.iter().any(|axis| ab.contains(axis))
}

/// The modifier source a binding's chord waits for, when it has a [`Trigger::Chord`].
fn chord_modifier<'a>(bindings: &'a [Binding], binding: &Binding) -> Option<&'a InputSource> {
    binding.triggers.iter().find_map(|t| match t {
        Trigger::Chord(helper) => bindings.iter().find(|b| &b.action == helper).map(|b| &b.input),
        _ => None,
    })
}

/// Which actions in `context` (other than `action` and its chord helper) bind `input`, with the
/// chord `modifier` taken into account: a chorded binding only conflicts with the same chord, a
/// plain binding only with plain bindings.
fn conflicts_of(
    config: &InputConfig,
    context: &str,
    action: &str,
    input: &InputSource,
    modifier: Option<&InputSource>,
) -> Vec<String> {
    let helper = chord_helper(action);
    let mut out: Vec<String> = config
        .contexts
        .get(context)
        .map(|ctx| {
            ctx.bindings
                .iter()
                .filter(|b| b.action != action && b.action != helper && !is_chord_helper(&b.action))
                .filter(|b| sources_overlap(&b.input, input))
                .filter(|b| chord_modifier(&ctx.bindings, b) == modifier)
                .map(|b| b.action.clone())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out.dedup();
    out
}

/// The other actions of `context` that share an input with any binding of `action` right now
/// (under the same chord), sorted: what a controls screen shows as a conflict, whether it came from
/// a rebind this session or from the saved overrides.
pub fn binding_conflicts(config: &InputConfig, context: &str, action: &str) -> Vec<String> {
    let Some(ctx) = config.contexts.get(context) else {
        return Vec::new();
    };
    let mut out: Vec<String> = ctx
        .bindings
        .iter()
        .filter(|b| b.action == action)
        .flat_map(|b| {
            conflicts_of(
                config,
                context,
                action,
                &b.input,
                chord_modifier(&ctx.bindings, b),
            )
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Turns the sources a finished capture collected into the bindings to write (the main binding
/// first, then a chord's modifier helper).
fn bindings_for(mode: RebindMode, slot: RebindSlot, action: &str, sources: Vec<InputSource>) -> Vec<Binding> {
    let key_at = |i: usize| match sources.get(i) {
        Some(InputSource::Key(key)) => key.clone(),
        _ => crate::KeyCode::new(""),
    };
    match mode {
        RebindMode::Single => sources
            .into_iter()
            .next()
            .map(|input| Binding::new(action, input))
            .into_iter()
            .collect(),
        RebindMode::Keys1D => vec![Binding::new(
            action,
            InputSource::Keys2D {
                up: crate::KeyCode::new(""),
                down: crate::KeyCode::new(""),
                left: key_at(0),
                right: key_at(1),
            },
        )],
        RebindMode::Keys2D => vec![Binding::new(
            action,
            InputSource::Keys2D {
                up: key_at(0),
                down: key_at(1),
                left: key_at(2),
                right: key_at(3),
            },
        )],
        RebindMode::Chord => {
            let helper = chord_helper_in(action, slot);
            let mut sources = sources.into_iter();
            match (sources.next(), sources.next()) {
                (Some(modifier), Some(main)) => vec![
                    Binding::new(action, main).with_trigger(Trigger::Chord(helper.clone())),
                    Binding::new(helper, modifier),
                ],
                _ => Vec::new(),
            }
        }
    }
}

/// Saves `overrides` through `vfs`, recording the result on `rebinder` (`persist_error`).
fn persist_overrides(
    overrides: &InputOverrides,
    vfs: Option<&Vfs>,
    rebinder: &mut InputRebinder,
    what: &str,
) {
    rebinder.persist_error = None;
    if rebinder.persist_blocked {
        let msg = "the existing input overrides file failed to load and was left untouched".to_string();
        tracing::warn!(target: "regia_input", "{what} not persisted: {msg}");
        rebinder.persist_error = Some(msg);
    } else if let Some(vfs) = vfs {
        let path = match VfsPath::parse(OVERRIDES_PATH) {
            Ok(path) => path,
            Err(err) => {
                tracing::error!(target: "regia_input", "overrides path: {err}");
                rebinder.persist_error = Some(format!("overrides path: {err}"));
                return;
            }
        };
        if let Err(err) = overrides.save(vfs, &path) {
            tracing::error!(target: "regia_input", "saving input overrides for {what} failed: {err}");
            rebinder.persist_error = Some(err);
        }
    } else {
        tracing::debug!(target: "regia_input", "no Vfs mounted; input overrides not persisted");
    }
}

/// `PreUpdate` (set [`crate::InputSet::Rebinding`], after the device updates, before
/// [`crate::evaluate_actions`]): captures the next raw input for a pending
/// [`InputRebinder`], writes it to the [`InputOverrides`] resource, merges it into the live
/// [`InputConfig`] and persists the overrides through the [`Vfs`] when one is mounted. A
/// multi-step mode ([`RebindMode`]) collects one input per press and rebinds after the last.
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
pub fn apply_rebinding(
    mut rebinder: ResMut<InputRebinder>,
    mut overrides: ResMut<InputOverrides>,
    mut config: ResMut<InputConfig>,
    keyboard: Res<Keyboard>,
    mouse: Res<Mouse>,
    gamepads: Res<Gamepads>,
    vfs: Option<Res<Vfs>>,
) {
    rebinder.update_guard(&keyboard, &mouse, &gamepads);
    let Some((context, action)) = rebinder.pending.clone() else {
        return;
    };
    if let Some(cancel) = rebinder
        .cancel_inputs
        .iter()
        .find(|input| press_edge(input, &keyboard, &gamepads))
        .cloned()
    {
        rebinder.cancel();
        rebinder.guard = Some(cancel);
        return;
    }
    // Axes held through `start` stay ignored until they drop below the threshold.
    let now = deflected_axes(&gamepads);
    let held = match rebinder.held_axes.take() {
        Some(previous) => previous.intersection(&now).copied().collect(),
        None => now,
    };
    let slot = rebinder.slot;
    let captured = match slot {
        RebindSlot::All => capture_source(&keyboard, &mouse, &gamepads, &held),
        RebindSlot::KeyboardMouse => capture_source(&keyboard, &mouse, &Gamepads::default(), &held),
        RebindSlot::Gamepad => capture_source(&Keyboard::default(), &Mouse::default(), &gamepads, &held),
    };
    rebinder.held_axes = Some(held);
    let kind = config.kind(&action);
    let captured = captured.or_else(|| {
        let listens = slot == RebindSlot::KeyboardMouse
            && rebinder.mode == RebindMode::Single
            && matches!(kind, Some(ActionKind::Axis2D | ActionKind::Axis3D));
        if !listens {
            return None;
        }
        rebinder.mouse_travel += mouse.delta().length();
        (rebinder.mouse_travel >= MOUSE_CAPTURE_TRAVEL).then_some(InputSource::MouseDelta)
    });
    let Some(input) = captured else {
        return;
    };
    let input = refine_analog(input, kind, &gamepads);
    let mode = rebinder.mode;
    let before = rebinder.captured.len();
    let finished = rebinder.accept(input.clone());
    if finished.is_some() || rebinder.captured.len() > before {
        rebinder.guard = Some(input);
    }
    let Some(sources) = finished else {
        return;
    };
    rebinder.pending = None;
    rebinder.held_axes = None;
    let mut bindings = bindings_for(mode, slot, &action, sources);
    let Some(main) = bindings.first_mut() else {
        return;
    };
    inherit_from_previous(main, &config, &context, slot, kind);
    let main = main.clone();
    let modifier = bindings.get(1).map(|b| b.input.clone());
    let conflicts = conflicts_of(&config, &context, &action, &main.input, modifier.as_ref());
    if !conflicts.is_empty() {
        tracing::warn!(
            target: "regia_input",
            "rebinding {action:?} to an input also used by {conflicts:?} in context {context:?}"
        );
    }
    if slot == RebindSlot::All {
        // A whole-action rebind supersedes earlier per-device ones.
        overrides.clear(&context, &action);
    }
    for binding in &bindings {
        match slot {
            RebindSlot::All => overrides.set(context.clone(), binding.action.clone(), binding.clone()),
            device => overrides.set_slot(
                context.clone(),
                binding.action.clone(),
                device,
                vec![binding.clone()],
            ),
        }
    }
    config.merge_overrides(&overrides);
    rebinder.outcome = Some(RebindOutcome {
        context,
        action: action.clone(),
        input: main.input.clone(),
        conflicts,
        slot,
    });
    rebinder.applied = bindings;
    persist_overrides(
        &overrides,
        vfs.as_deref(),
        &mut rebinder,
        &format!("rebinding {action:?}"),
    );
}

/// Whether `input` (a key or a pad button) was pressed this frame.
fn press_edge(input: &InputSource, keyboard: &Keyboard, gamepads: &Gamepads) -> bool {
    match input {
        InputSource::Key(key) => keyboard.just_pressed.contains(key),
        InputSource::GamepadButton(button) => gamepads
            .pads()
            .any(|(_, pad)| pad.buttons.get(button).is_some_and(|b| b.just_pressed)),
        _ => false,
    }
}

/// Whether `input` is down or went up this frame. Analog inputs never send a click: false.
fn press_alive(input: &InputSource, keyboard: &Keyboard, mouse: &Mouse, gamepads: &Gamepads) -> bool {
    match input {
        InputSource::Key(key) => keyboard.pressed(key.as_str()) || keyboard.just_released.contains(key),
        InputSource::MouseButton(button) => mouse.pressed(*button) || mouse.just_released.contains(button),
        InputSource::GamepadButton(button) => gamepads.pads().any(|(_, pad)| {
            pad.buttons
                .get(button)
                .is_some_and(|b| b.value > crate::gamepad::BUTTON_THRESHOLD || b.just_released)
        }),
        _ => false,
    }
}

impl InputRebinder {
    /// Ends [`Self::guard`] on the first pass after its input came up.
    fn update_guard(&mut self, keyboard: &Keyboard, mouse: &Mouse, gamepads: &Gamepads) {
        if self
            .guard
            .as_ref()
            .is_some_and(|input| !press_alive(input, keyboard, mouse, gamepads))
        {
            self.guard = None;
        }
    }
}

/// A stick deflection captured for an action that reads one value: the single axis that moved,
/// so a push up binds the stick's Y axis instead of a stick whose X the action would read.
fn refine_analog(input: InputSource, kind: Option<ActionKind>, gamepads: &Gamepads) -> InputSource {
    let InputSource::GamepadStick(stick) = &input else {
        return input;
    };
    if !matches!(kind, Some(ActionKind::Axis1D | ActionKind::Button)) {
        return input;
    }
    let magnitude = |axis| {
        gamepads
            .pads()
            .filter_map(|(_, pad)| pad.axes.get(&axis))
            .fold(0.0f32, |peak, value| peak.max(value.abs()))
    };
    let axis = if magnitude(stick.y()) > magnitude(stick.x()) {
        stick.y()
    } else {
        stick.x()
    };
    InputSource::GamepadAxis(axis)
}

/// The dead zone a captured stick or pad axis gets when no earlier binding gave it one.
pub const DEFAULT_DEAD_ZONE: f32 = 0.15;
/// The dead zone of a stick or pad axis bound to a button action: past half travel it is pressed.
pub const BUTTON_AXIS_DEAD_ZONE: f32 = 0.5;

/// What kind of value a source produces; modifiers carry over only within one shape (a mouse
/// sensitivity scale means nothing on a stick).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceShape {
    Digital,
    KeyGroup,
    Mouse,
    Stick,
    Axis,
}

fn shape(source: &InputSource) -> SourceShape {
    match source {
        InputSource::Key(_) | InputSource::MouseButton(_) | InputSource::GamepadButton(_) => {
            SourceShape::Digital
        }
        InputSource::Keys2D { .. } => SourceShape::KeyGroup,
        InputSource::MouseDelta => SourceShape::Mouse,
        InputSource::GamepadStick(_) => SourceShape::Stick,
        InputSource::GamepadAxis(_) => SourceShape::Axis,
    }
}

/// Carries the action's meaning over to a newly captured binding: the triggers of its previous
/// binding (a fire action stays `Pressed`; a chord belongs to the capture mode, not the action) and
/// the modifiers of a previous binding of the same shape on the slot's devices. A stick or pad axis
/// always ends up with a dead zone, and a stick's Y axis on a one-value action is moved to x, where
/// the action reads it.
fn inherit_from_previous(
    main: &mut Binding,
    config: &InputConfig,
    context: &str,
    slot: RebindSlot,
    kind: Option<ActionKind>,
) {
    let previous: Vec<&Binding> = config
        .contexts
        .get(context)
        .map(|ctx| ctx.bindings.iter().filter(|b| b.action == main.action).collect())
        .unwrap_or_default();
    if let Some(first) = previous.first() {
        let mut triggers: Vec<Trigger> = first
            .triggers
            .iter()
            .filter(|t| !matches!(t, Trigger::Chord(_)))
            .cloned()
            .collect();
        triggers.append(&mut main.triggers);
        main.triggers = triggers;
    }
    let same_shape = previous
        .iter()
        .find(|b| slot.holds(&b.input) && shape(&b.input) == shape(&main.input));
    if let Some(same) = same_shape {
        main.modifiers.clone_from(&same.modifiers);
    }
    let analog = matches!(shape(&main.input), SourceShape::Stick | SourceShape::Axis);
    if !analog {
        return;
    }
    main.modifiers
        .retain(|m| !matches!(m, Modifier::Swizzle(Swizzle::Yxz)));
    let on_y = matches!(
        main.input,
        InputSource::GamepadAxis(
            regia_platform::GamepadAxis::LeftStickY | regia_platform::GamepadAxis::RightStickY
        )
    );
    if on_y && kind == Some(ActionKind::Axis1D) {
        main.modifiers.insert(0, Modifier::Swizzle(Swizzle::Yxz));
    }
    let button = kind == Some(ActionKind::Button);
    let zone = if button {
        BUTTON_AXIS_DEAD_ZONE
    } else {
        DEFAULT_DEAD_ZONE
    };
    if !main.modifiers.iter().any(|m| matches!(m, Modifier::DeadZone(_))) {
        let dead_zone = if shape(&main.input) == SourceShape::Stick {
            DeadZone::radial(zone, 1.0)
        } else {
            DeadZone::axial(zone, if button { zone } else { 1.0 })
        };
        main.modifiers.insert(0, Modifier::DeadZone(dead_zone));
    }
}

/// Why a rebind operation was refused. [`RebindError::code`] is the lowercase protocol code
/// (`book/agents/input-rebind.md`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RebindError {
    /// The app has no [`InputPlugin`](crate::InputPlugin) resources.
    #[error("the input layer is not installed")]
    NotInstalled,
    /// The config has no context of this name.
    #[error("no input context named `{0}`")]
    UnknownContext(String),
    /// The context has no binding for, and the config does not declare, this action.
    #[error("no action named `{0}` in that context")]
    UnknownAction(String),
    /// The capture mode does not fit the action's kind.
    #[error("capture mode `{mode}` does not fit the {kind:?} action `{action}`")]
    BadMode {
        /// The action.
        action: String,
        /// The refused mode.
        mode: &'static str,
        /// The action's kind.
        kind: ActionKind,
    },
    /// No rebind capture is pending.
    #[error("no rebind capture is pending")]
    NotPending,
    /// The project config has no default binding to restore.
    #[error("the project config has no default binding for `{0}`")]
    NoDefault(String),
    /// A dead zone outside `0..=`[`MAX_DEAD_ZONE`], or not finite.
    #[error("dead zone {0} is outside 0..=0.95")]
    BadTuning(String),
    /// The slot holds no binding a dead zone or invert applies to (a stick, pad axis, key group
    /// or mouse motion).
    #[error("`{0}` has no axis binding on that device slot")]
    NotTunable(String),
    /// A profile name other than 1 to 32 of `a-z`, `0-9`, `_`, `-`.
    #[error("bad profile name `{0}`: use 1 to 32 of a-z, 0-9, _ and -")]
    BadProfile(String),
    /// The profile file does not exist, or no `Vfs` is mounted.
    #[error("no input profile at `{0}`")]
    ProfileMissing(String),
    /// The profile file does not load; nothing changed.
    #[error("input profile does not load: {0}")]
    ProfileCorrupt(String),
    /// The overrides could not be written.
    #[error("input overrides not saved: {0}")]
    SaveFailed(String),
}

impl RebindError {
    /// The lowercase protocol code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotInstalled => "input_not_installed",
            Self::UnknownContext(_) => "rebind_unknown_context",
            Self::UnknownAction(_) => "rebind_unknown_action",
            Self::BadMode { .. } => "rebind_bad_mode",
            Self::NotPending => "rebind_not_pending",
            Self::NoDefault(_) => "rebind_no_default",
            Self::BadTuning(_) => "rebind_bad_tuning",
            Self::NotTunable(_) => "rebind_not_tunable",
            Self::BadProfile(_) => "rebind_bad_profile",
            Self::ProfileMissing(_) => "rebind_profile_missing",
            Self::ProfileCorrupt(_) => "rebind_profile_corrupt",
            Self::SaveFailed(_) => "rebind_save_failed",
        }
    }
}

/// The project config's own bindings (before the user overrides were merged), kept so single
/// bindings can be reset ([`reset_action`]). Installed by [`crate::InputPlugin`].
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct InputDefaults(pub InputConfig);

/// Starts a rebind capture of `action` in `context` in `mode`, after checking that the context
/// and action exist and that the mode fits the action's kind.
pub fn start_rebind(
    world: &mut World,
    context: &str,
    action: &str,
    mode: RebindMode,
) -> Result<(), RebindError> {
    start_rebind_slot(world, context, action, mode, RebindSlot::All)
}

/// [`start_rebind`] on one device slot ([`InputRebinder::start_slot`]); a key group on the gamepad
/// slot is refused as a bad mode.
pub fn start_rebind_slot(
    world: &mut World,
    context: &str,
    action: &str,
    mode: RebindMode,
    slot: RebindSlot,
) -> Result<(), RebindError> {
    let config = world
        .get_resource::<InputConfig>()
        .ok_or(RebindError::NotInstalled)?;
    let ctx = config
        .contexts
        .get(context)
        .ok_or_else(|| RebindError::UnknownContext(context.to_string()))?;
    let kind = config
        .kind(action)
        .filter(|_| !is_chord_helper(action) && ctx.bindings.iter().any(|b| b.action == action))
        .ok_or_else(|| RebindError::UnknownAction(action.to_string()))?;
    if !mode.fits(kind) || !slot.fits(mode) {
        return Err(RebindError::BadMode {
            action: action.to_string(),
            mode: mode.name(),
            kind,
        });
    }
    let mut rebinder = world
        .get_resource_mut::<InputRebinder>()
        .ok_or(RebindError::NotInstalled)?;
    rebinder.start_slot(context, action, mode, slot);
    Ok(())
}

/// Cancels the pending capture; errors when none is pending.
pub fn cancel_rebind(world: &mut World) -> Result<(), RebindError> {
    let mut rebinder = world
        .get_resource_mut::<InputRebinder>()
        .ok_or(RebindError::NotInstalled)?;
    if rebinder.pending().is_none() {
        return Err(RebindError::NotPending);
    }
    rebinder.cancel();
    Ok(())
}

/// Restores `action`'s project-config bindings in `context` (and drops its chord helper), removes
/// its override and saves. Returns whether there was an override to remove.
pub fn reset_action(world: &mut World, context: &str, action: &str) -> Result<bool, RebindError> {
    let defaults = world
        .get_resource::<InputDefaults>()
        .ok_or(RebindError::NotInstalled)?
        .0
        .clone();
    let default_ctx = defaults
        .contexts
        .get(context)
        .ok_or_else(|| RebindError::UnknownContext(context.to_string()))?;
    let helpers = [RebindSlot::All, RebindSlot::KeyboardMouse, RebindSlot::Gamepad]
        .map(|slot| chord_helper_in(action, slot));
    let wanted = |name: &str| name == action || helpers.iter().any(|helper| helper == name);
    if !default_ctx.bindings.iter().any(|b| b.action == action) {
        return Err(RebindError::NoDefault(action.to_string()));
    }
    let restore: Vec<Binding> = default_ctx
        .bindings
        .iter()
        .filter(|b| wanted(&b.action))
        .cloned()
        .collect();
    let mut removed = false;
    {
        let mut overrides = world
            .get_resource_mut::<InputOverrides>()
            .ok_or(RebindError::NotInstalled)?;
        removed |= overrides.clear(context, action);
        for helper in &helpers {
            removed |= overrides.clear(context, helper);
        }
    }
    {
        let mut config = world
            .get_resource_mut::<InputConfig>()
            .ok_or(RebindError::NotInstalled)?;
        let ctx = config
            .contexts
            .get_mut(context)
            .ok_or_else(|| RebindError::UnknownContext(context.to_string()))?;
        ctx.bindings.retain(|b| !wanted(&b.action));
        ctx.bindings.extend(restore);
        for (name, kind) in &defaults.actions {
            config.actions.entry(name.clone()).or_insert(*kind);
        }
    }
    save_overrides(world, &format!("reset of {action:?}"));
    Ok(removed)
}

/// Drops every override and restores the whole project config's contexts, then saves. Returns how
/// many overridden actions were removed.
pub fn reset_all(world: &mut World) -> Result<usize, RebindError> {
    let defaults = world
        .get_resource::<InputDefaults>()
        .ok_or(RebindError::NotInstalled)?
        .0
        .clone();
    let count = {
        let mut overrides = world
            .get_resource_mut::<InputOverrides>()
            .ok_or(RebindError::NotInstalled)?;
        let whole: usize = overrides.contexts.values().map(BTreeMap::len).sum();
        let slotted: usize = overrides.slots.values().map(BTreeMap::len).sum();
        overrides.contexts.clear();
        overrides.slots.clear();
        whole + slotted
    };
    {
        let mut config = world
            .get_resource_mut::<InputConfig>()
            .ok_or(RebindError::NotInstalled)?;
        for (name, default_ctx) in &defaults.contexts {
            if let Some(ctx) = config.contexts.get_mut(name) {
                ctx.bindings = default_ctx.bindings.clone();
            }
        }
        for (name, kind) in &defaults.actions {
            config.actions.entry(name.clone()).or_insert(*kind);
        }
    }
    save_overrides(world, "reset of all bindings");
    Ok(count)
}

/// The largest dead zone [`set_axis_tuning`] accepts: above it a stick would barely respond.
pub const MAX_DEAD_ZONE: f32 = 0.95;

/// A player's settings for one axis on one device slot (ADR-2129).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AxisTuning {
    /// Lower edge of the dead zone on the slot's sticks and pad axes; `None` keeps (when setting)
    /// or reports (when reading) no dead zone. Keys and mouse motion have none.
    pub dead_zone: Option<f32>,
    /// Negate the action's x value.
    pub invert_x: bool,
    /// Negate the action's y value.
    pub invert_y: bool,
}

/// Whether a dead zone or invert applies to `source`: buttons have no axis to invert.
fn tunable(source: &InputSource) -> bool {
    !matches!(shape(source), SourceShape::Digital)
}

/// What `binding`'s modifiers mean as an [`AxisTuning`].
fn tuning_of(binding: &Binding) -> AxisTuning {
    let mut tuning = AxisTuning::default();
    for modifier in &binding.modifiers {
        match modifier {
            Modifier::DeadZone(zone) => tuning.dead_zone = Some(zone.lo),
            Modifier::Negate { x, y, .. } => {
                tuning.invert_x ^= x;
                tuning.invert_y ^= y;
            }
            Modifier::InvertY(invert) => tuning.invert_y ^= invert,
            _ => {}
        }
    }
    tuning
}

/// The tuning of `action` on `slot`, read from its first axis binding there; the dead zone from its
/// first stick or pad axis, as nothing else has one. `None` when it has no axis binding on the slot.
pub fn axis_tuning(
    config: &InputConfig,
    context: &str,
    action: &str,
    slot: RebindSlot,
) -> Option<AxisTuning> {
    let bindings = &config.contexts.get(context)?.bindings;
    let mut axes = bindings
        .iter()
        .filter(|b| b.action == action && slot.holds(&b.input) && tunable(&b.input));
    let mut tuning = tuning_of(axes.next()?);
    if tuning.dead_zone.is_none() {
        tuning.dead_zone = axes
            .find(|b| matches!(shape(&b.input), SourceShape::Stick | SourceShape::Axis))
            .and_then(|b| tuning_of(b).dead_zone);
    }
    Some(tuning)
}

/// Whether `action` has a stick or pad axis on `slot`: only those have a dead zone.
pub fn has_dead_zone_axis(config: &InputConfig, context: &str, action: &str, slot: RebindSlot) -> bool {
    config.contexts.get(context).is_some_and(|ctx| {
        ctx.bindings.iter().any(|b| {
            b.action == action
                && slot.holds(&b.input)
                && matches!(shape(&b.input), SourceShape::Stick | SourceShape::Axis)
        })
    })
}

/// `binding` with `tuning` applied: the dead zone (on sticks and pad axes) edited in place or put
/// first, invert as a last [`Modifier::Negate`] so it acts on the action's value. Every other
/// modifier stays where it is.
fn apply_tuning(binding: &mut Binding, tuning: AxisTuning) {
    let analog = matches!(shape(&binding.input), SourceShape::Stick | SourceShape::Axis);
    if analog {
        match tuning.dead_zone {
            None => binding.modifiers.retain(|m| !matches!(m, Modifier::DeadZone(_))),
            Some(lo) => {
                let existing = binding.modifiers.iter_mut().find_map(|m| match m {
                    Modifier::DeadZone(zone) => Some(zone),
                    _ => None,
                });
                match existing {
                    Some(zone) => {
                        zone.lo = lo;
                        zone.hi = zone.hi.max(lo);
                    }
                    None => binding.modifiers.insert(
                        0,
                        Modifier::DeadZone(if shape(&binding.input) == SourceShape::Stick {
                            DeadZone::radial(lo, 1.0)
                        } else {
                            DeadZone::axial(lo, 1.0)
                        }),
                    ),
                }
            }
        }
    }
    let z = binding.modifiers.iter().fold(false, |z, m| match m {
        Modifier::Negate { z: flip, .. } => z ^ flip,
        _ => z,
    });
    binding
        .modifiers
        .retain(|m| !matches!(m, Modifier::Negate { .. } | Modifier::InvertY(_)));
    if tuning.invert_x || tuning.invert_y || z {
        binding.modifiers.push(Modifier::Negate {
            x: tuning.invert_x,
            y: tuning.invert_y,
            z,
        });
    }
}

/// Sets the dead zone and invert of `action`'s axis bindings on `slot` ([`RebindSlot::All`]: on
/// every device), merges them over the config as per-device overrides and saves. Returns the
/// tuning now in effect.
pub fn set_axis_tuning(
    world: &mut World,
    context: &str,
    action: &str,
    slot: RebindSlot,
    tuning: AxisTuning,
) -> Result<AxisTuning, RebindError> {
    if let Some(lo) = tuning.dead_zone
        && !(0.0..=MAX_DEAD_ZONE).contains(&lo)
    {
        return Err(RebindError::BadTuning(lo.to_string()));
    }
    let config = world
        .get_resource::<InputConfig>()
        .ok_or(RebindError::NotInstalled)?;
    let ctx = config
        .contexts
        .get(context)
        .ok_or_else(|| RebindError::UnknownContext(context.to_string()))?;
    if is_chord_helper(action) || !ctx.bindings.iter().any(|b| b.action == action) {
        return Err(RebindError::UnknownAction(action.to_string()));
    }
    let mut writes: Vec<(RebindSlot, Vec<Binding>)> = Vec::new();
    for device in [RebindSlot::KeyboardMouse, RebindSlot::Gamepad] {
        if slot != RebindSlot::All && slot != device {
            continue;
        }
        let on_device: Vec<&Binding> = ctx
            .bindings
            .iter()
            .filter(|b| b.action == action && device.holds(&b.input))
            .collect();
        if !on_device.iter().any(|b| tunable(&b.input)) {
            continue;
        }
        let tuned = on_device
            .into_iter()
            .map(|b| {
                let mut binding = b.clone();
                if tunable(&binding.input) {
                    apply_tuning(&mut binding, tuning);
                }
                binding
            })
            .collect();
        writes.push((device, tuned));
    }
    if writes.is_empty() {
        return Err(RebindError::NotTunable(action.to_string()));
    }
    {
        let mut overrides = world
            .get_resource_mut::<InputOverrides>()
            .ok_or(RebindError::NotInstalled)?;
        for (device, bindings) in writes {
            overrides.set_slot(context, action, device, bindings);
        }
    }
    let overrides = world
        .get_resource::<InputOverrides>()
        .ok_or(RebindError::NotInstalled)?
        .clone();
    let mut config = world
        .get_resource_mut::<InputConfig>()
        .ok_or(RebindError::NotInstalled)?;
    config.merge_overrides(&overrides);
    let now = axis_tuning(&config, context, action, slot).ok_or(RebindError::NotInstalled)?;
    save_overrides(world, &format!("tuning of {action:?}"));
    Ok(now)
}

/// The path of a named profile, or of the active overrides file when `name` is `None`.
pub fn profile_path(name: Option<&str>) -> Result<VfsPath, RebindError> {
    let text = match name {
        None => OVERRIDES_PATH.to_string(),
        Some(name) => {
            let good = (1..=32).contains(&name.len())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
            if !good {
                return Err(RebindError::BadProfile(name.to_string()));
            }
            format!("{PROFILES_DIR}/{name}.ron")
        }
    };
    VfsPath::parse(&text).map_err(|e| RebindError::SaveFailed(format!("{text}: {e}")))
}

/// Writes the live overrides to a named profile, or to the active file when `name` is `None`.
/// Returns the path. The active file is refused while [`InputRebinder::persist_blocked`] holds (it
/// failed to load and is left for the player to look at); a named copy is not.
pub fn save_profile(world: &World, name: Option<&str>) -> Result<String, RebindError> {
    let path = profile_path(name)?;
    let overrides = world
        .get_resource::<InputOverrides>()
        .ok_or(RebindError::NotInstalled)?;
    if name.is_none()
        && world
            .get_resource::<InputRebinder>()
            .is_some_and(InputRebinder::persist_blocked)
    {
        return Err(RebindError::SaveFailed(
            "the existing input overrides file failed to load and was left untouched".to_string(),
        ));
    }
    let vfs = world
        .get_resource::<Vfs>()
        .ok_or_else(|| RebindError::SaveFailed("no Vfs is mounted".to_string()))?;
    overrides.save(vfs, &path).map_err(RebindError::SaveFailed)?;
    Ok(path.to_string())
}

/// Replaces the live overrides with a named profile (or the active file when `name` is `None`):
/// the project bindings come back, the profile merges over them, and a named profile becomes the
/// active one (written to `user://input_overrides.ron`, so it survives a restart). A missing or
/// corrupt file changes nothing. Returns how many overrides the profile holds.
pub fn load_profile(world: &mut World, name: Option<&str>) -> Result<usize, RebindError> {
    let path = profile_path(name)?;
    let vfs = world
        .get_resource::<Vfs>()
        .cloned()
        .ok_or_else(|| RebindError::ProfileMissing(path.to_string()))?;
    if !vfs.exists(&path) {
        return Err(RebindError::ProfileMissing(path.to_string()));
    }
    let loaded = InputOverrides::load(&vfs, &path).map_err(RebindError::ProfileCorrupt)?;
    let defaults = world
        .get_resource::<InputDefaults>()
        .ok_or(RebindError::NotInstalled)?
        .0
        .clone();
    {
        let mut config = world
            .get_resource_mut::<InputConfig>()
            .ok_or(RebindError::NotInstalled)?;
        for (context, default_ctx) in &defaults.contexts {
            if let Some(ctx) = config.contexts.get_mut(context) {
                ctx.bindings.clone_from(&default_ctx.bindings);
            }
        }
        config.merge_overrides(&loaded);
    }
    let count = loaded.contexts.values().map(BTreeMap::len).sum::<usize>()
        + loaded.slots.values().map(BTreeMap::len).sum::<usize>();
    world.insert_resource(loaded);
    if name.is_some() {
        // The player chose this set: it replaces whatever the active file held, even a corrupt one.
        if let Some(mut rebinder) = world.get_resource_mut::<InputRebinder>() {
            rebinder.allow_persist();
        }
        save_overrides(world, "load of an input profile");
    }
    Ok(count)
}

/// The names of the saved profiles, sorted. Empty without a `Vfs` or before the first save.
pub fn list_profiles(world: &World) -> Vec<String> {
    let Some(vfs) = world.get_resource::<Vfs>() else {
        return Vec::new();
    };
    let Ok(dir) = VfsPath::parse(PROFILES_DIR) else {
        return Vec::new();
    };
    let mut names: Vec<String> = vfs
        .list_dir(&dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| entry.name.strip_suffix(".ron").map(str::to_string))
        .filter(|name| profile_path(Some(name)).is_ok())
        .collect();
    names.sort();
    names
}

/// Saves the live [`InputOverrides`] through the world's [`Vfs`], if any.
fn save_overrides(world: &mut World, what: &str) {
    let Some(overrides) = world.get_resource::<InputOverrides>().cloned() else {
        return;
    };
    let vfs = world.get_resource::<Vfs>().cloned();
    if let Some(mut rebinder) = world.get_resource_mut::<InputRebinder>() {
        persist_overrides(&overrides, vfs.as_ref(), &mut rebinder, what);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_order_handles_large_other_buttons() {
        use regia_platform::MouseButton as B;
        assert_eq!(button_order(B::Other(u16::MAX)), 1000 + u32::from(u16::MAX));
        assert!(button_order(B::Other(64536)) > button_order(B::Other(1)));
    }

    #[test]
    fn load_reports_invalid_utf8_with_one_path_prefix() {
        use regia_platform::{DirMount, MountPoint};
        let dir = std::env::temp_dir().join(format!("regia-input-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("input_overrides.ron"), [b'(', 0xFF, 0xFE]).expect("write");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(dir.clone()));
        let path = VfsPath::parse(OVERRIDES_PATH).expect("path");
        let err = InputOverrides::load(&vfs, &path).expect_err("invalid utf-8");
        assert!(err.contains("invalid UTF-8 at byte 1"), "{err}");
        assert_eq!(err.matches("input_overrides.ron").count(), 1, "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overrides_round_trip_through_ron() {
        let mut overrides = InputOverrides::default();
        overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
        let text = overrides.to_ron();
        let parsed = InputOverrides::from_ron(&text).expect("parses");
        assert_eq!(overrides, parsed);
        assert!(!InputOverrides::default().to_ron().is_empty());
    }

    #[test]
    fn try_to_ron_matches_to_ron() {
        let mut overrides = InputOverrides::default();
        overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
        let ok = overrides.try_to_ron().expect("serializes");
        assert_eq!(ok, overrides.to_ron());
        assert_eq!(InputOverrides::from_ron(&ok).expect("parses"), overrides);
    }

    fn pad_with_axis(axis: regia_platform::GamepadAxis, value: f32) -> Gamepads {
        let mut pad = crate::gamepad::Pad {
            name: "Test Pad".into(),
            ..Default::default()
        };
        pad.axes.insert(axis, value);
        let mut gamepads = Gamepads::default();
        gamepads.pads.insert(1, pad);
        gamepads
    }

    #[test]
    fn axis_held_through_start_is_not_captured_until_released() {
        use regia_platform::GamepadAxis as A;
        let (keyboard, mouse) = (Keyboard::default(), Mouse::default());
        let mut held = BTreeSet::new();
        // Stick already deflected when the capture begins: baseline it.
        let gamepads = pad_with_axis(A::LeftStickX, 0.9);
        held.extend(deflected_axes(&gamepads));
        assert_eq!(capture_source(&keyboard, &mouse, &gamepads, &held), None);
        // Released, then pushed again: a fresh deflection is captured.
        let rested = pad_with_axis(A::LeftStickX, 0.0);
        let now = deflected_axes(&rested);
        held = held.intersection(&now).copied().collect();
        assert!(held.is_empty());
        let pushed = pad_with_axis(A::LeftStickX, 0.9);
        assert_eq!(
            capture_source(&keyboard, &mouse, &pushed, &held),
            Some(InputSource::GamepadStick(crate::Stick::Left))
        );
    }

    #[test]
    fn conflict_detection_finds_other_actions_with_the_same_source() {
        let config = crate::config::InputConfig::new()
            .action("Jump", crate::ActionKind::Button)
            .action("Crouch", crate::ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new("Crouch", InputSource::key("Space")))
            .binding(Binding::new("Jump", InputSource::key("KeyJ")));
        let conflicts = conflicts_of(&config, "Gameplay", "Jump", &InputSource::key("Space"), None);
        assert_eq!(conflicts, vec!["Crouch".to_string()]);
        assert!(conflicts_of(&config, "Gameplay", "Jump", &InputSource::key("KeyQ"), None).is_empty());
    }

    fn keyboard_just(keys: &[&str]) -> Keyboard {
        let mut keyboard = Keyboard::default();
        for key in keys {
            keyboard.just_pressed.insert(crate::KeyCode::new(*key));
        }
        keyboard
    }

    fn mouse_just(buttons: &[regia_platform::MouseButton]) -> Mouse {
        let mut mouse = Mouse::default();
        for button in buttons {
            mouse.just_pressed.insert(*button);
        }
        mouse
    }

    fn named_pad() -> crate::gamepad::Pad {
        crate::gamepad::Pad {
            name: "pad".into(),
            ..crate::gamepad::Pad::default()
        }
    }

    #[test]
    fn capture_order_and_axis_threshold() {
        use regia_platform::{GamepadAxis, GamepadButton, MouseButton};

        let none = BTreeSet::new();
        let keyboard = keyboard_just(&["KeyZ", "KeyA"]);
        let mouse = mouse_just(&[MouseButton::Right, MouseButton::Left, MouseButton::Other(1)]);
        assert_eq!(
            capture_source(&keyboard, &mouse, &Gamepads::default(), &none),
            Some(InputSource::key("KeyA")),
            "keys sort by name and outrank every other device"
        );

        let mouse = mouse_just(&[MouseButton::Other(2), MouseButton::Forward, MouseButton::Middle]);
        assert_eq!(
            capture_source(&Keyboard::default(), &mouse, &Gamepads::default(), &none),
            Some(InputSource::mouse_button(MouseButton::Middle))
        );

        let mut pads = Gamepads::default();
        let mut unnamed = crate::gamepad::Pad::default();
        unnamed.buttons.insert(
            GamepadButton::South,
            crate::gamepad::PadButton {
                value: 1.0,
                just_pressed: true,
                ..crate::gamepad::PadButton::default()
            },
        );
        pads.pads.insert(1, unnamed);
        assert_eq!(
            capture_source(&Keyboard::default(), &Mouse::default(), &pads, &none),
            None,
            "a pad with no name is not connected"
        );

        let mut low = named_pad();
        low.buttons.insert(
            GamepadButton::North,
            crate::gamepad::PadButton {
                value: 1.0,
                just_pressed: true,
                ..crate::gamepad::PadButton::default()
            },
        );
        low.axes.insert(GamepadAxis::RightStickX, 1.0);
        let mut high = named_pad();
        high.buttons.insert(
            GamepadButton::South,
            crate::gamepad::PadButton {
                value: 1.0,
                just_pressed: true,
                ..crate::gamepad::PadButton::default()
            },
        );
        let mut pads = Gamepads::default();
        pads.pads.insert(2, low);
        pads.pads.insert(8, high);
        assert_eq!(
            capture_source(&Keyboard::default(), &Mouse::default(), &pads, &none),
            Some(InputSource::gamepad_button(GamepadButton::North)),
            "the lower pad id wins, and a button outranks a deflected stick"
        );

        let mut one = named_pad();
        for button in [GamepadButton::North, GamepadButton::South] {
            one.buttons.insert(
                button,
                crate::gamepad::PadButton {
                    value: 1.0,
                    just_pressed: true,
                    ..crate::gamepad::PadButton::default()
                },
            );
        }
        let mut pads = Gamepads::default();
        pads.pads.insert(3, one);
        assert_eq!(
            capture_source(&Keyboard::default(), &Mouse::default(), &pads, &none),
            Some(InputSource::gamepad_button(GamepadButton::South))
        );

        let (keyboard, mouse) = (Keyboard::default(), Mouse::default());
        assert_eq!(
            capture_source(
                &keyboard,
                &mouse,
                &pad_with_axis(GamepadAxis::LeftStickX, 0.6),
                &none
            ),
            None,
            "0.6 is not past the capture threshold"
        );
        assert!(deflected_axes(&pad_with_axis(GamepadAxis::LeftStickX, 0.6)).is_empty());
        assert_eq!(
            capture_source(
                &keyboard,
                &mouse,
                &pad_with_axis(GamepadAxis::LeftStickX, -0.75),
                &none
            ),
            Some(InputSource::GamepadStick(crate::Stick::Left))
        );
        assert_eq!(
            capture_source(
                &keyboard,
                &mouse,
                &pad_with_axis(GamepadAxis::RightStickY, 0.75),
                &none
            ),
            Some(InputSource::GamepadStick(crate::Stick::Right))
        );
        assert_eq!(
            capture_source(
                &keyboard,
                &mouse,
                &pad_with_axis(GamepadAxis::LeftTrigger, 1.0),
                &none
            ),
            Some(InputSource::GamepadAxis(GamepadAxis::LeftTrigger))
        );
        assert_eq!(
            capture_source(
                &keyboard,
                &mouse,
                &pad_with_axis(GamepadAxis::DPadY, -0.75),
                &none
            ),
            Some(InputSource::GamepadAxis(GamepadAxis::DPadY))
        );

        let mut both = named_pad();
        both.axes.insert(GamepadAxis::RightStickX, 0.9);
        both.axes.insert(GamepadAxis::LeftStickY, -0.9);
        let mut pads = Gamepads::default();
        pads.pads.insert(1, both);
        let held = BTreeSet::from([(1, GamepadAxis::LeftStickY)]);
        assert_eq!(
            capture_source(&keyboard, &mouse, &pads, &held),
            Some(InputSource::GamepadStick(crate::Stick::Right)),
            "an axis held through the start is skipped; the other stick is taken"
        );
        assert_eq!(
            deflected_axes(&pads),
            BTreeSet::from([(1, GamepadAxis::LeftStickY), (1, GamepadAxis::RightStickX)])
        );
    }
}
