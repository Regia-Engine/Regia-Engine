// SPDX-License-Identifier: MPL-2.0
//! Action-based input (spec 58 § 2.7): the action manifest (`assets/xr/actions.xr.ron`), the
//! attach-once freeze rule, the per-sync action states, the mapping onto `regia_input`, and haptics.
//!
//! The model is the OpenXR standard's: the app declares action sets and typed actions, suggests
//! bindings per interaction profile, attaches the sets once per session, then syncs once per fixed
//! step and reads the synced values (two reads between syncs are identical). While the session is
//! not live (the session is neither running nor focused) every action reads inactive with its
//! default value.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::glam::{Vec2, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::{ActionValue, ControllerAction, ControllerState, Hand, Pose, XrBackend, XrError};
use serde::{Deserialize, Serialize};

use crate::comfort::DominantHand;

/// The `format_version` this build writes and the newest it reads.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// Where a project keeps its manifest (asset path).
pub const MANIFEST_PATH: &str = "assets/xr/actions.xr.ron";

/// The sub-action paths an action may name (`/user/hand/left`, `/user/hand/right`).
pub const SUB_PATHS: [&str; 2] = ["left", "right"];

/// An action's value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Reflect)]
pub enum ActionType {
    /// A button (published to `regia_input` as a button).
    Bool,
    /// An analog axis, zero to one (published as an axis).
    Float,
    /// A two-axis stick (published as a 2D axis).
    Vec2,
    /// A pose (grip, aim): published to [`ActionPoses`].
    Pose,
    /// A haptic output.
    Haptic,
}

/// One action of a set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reflect)]
#[serde(deny_unknown_fields)]
pub struct ActionDecl {
    /// Stable name, `[a-z][a-z0-9_]*`, unique in its set.
    pub name: String,
    /// The value type.
    pub ty: ActionType,
    /// Localization key of the label the runtime's rebinding UI shows (empty: the name).
    #[serde(default)]
    pub label: String,
    /// Sub-action paths (`left`, `right`); empty means the action is not per hand.
    #[serde(default)]
    pub subs: Vec<String>,
}

/// An action set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reflect)]
#[serde(deny_unknown_fields)]
pub struct ActionSetDecl {
    /// Stable name, `[a-z][a-z0-9_]*`, unique in the manifest.
    pub name: String,
    /// Priority when two sets bind the same input (higher wins).
    #[serde(default)]
    pub priority: u32,
    /// The set's actions.
    pub actions: Vec<ActionDecl>,
}

/// One suggested binding: `(action, sub-path, input path)`. A relative input path
/// (`select/click`) is under `/user/hand/<sub>/input/`; an absolute one (starting with `/`) is the
/// standard's full path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reflect)]
pub struct BindingDecl(pub String, pub String, pub String);

impl BindingDecl {
    /// The action name.
    pub fn action(&self) -> &str {
        &self.0
    }

    /// The sub-action path.
    pub fn sub(&self) -> &str {
        &self.1
    }

    /// The full input path (`/user/hand/left/input/select/click`).
    pub fn full_path(&self) -> String {
        if self.2.starts_with('/') {
            self.2.clone()
        } else {
            format!("/user/hand/{}/input/{}", self.1, self.2)
        }
    }
}

/// The suggested bindings for one interaction profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reflect)]
#[serde(deny_unknown_fields)]
pub struct ProfileBindings {
    /// The interaction profile (the standard's profile path, or a short name such as
    /// `simple_controller`).
    pub profile: String,
    /// Intended for vendor profiles that are suggested only when the runtime advertises their
    /// extension. Stored only: no suggestion step reads it until the OpenXR binding exists.
    #[serde(default)]
    pub optional: bool,
    /// The bindings.
    pub map: Vec<BindingDecl>,
}

/// The action manifest (`assets/xr/actions.xr.ron`, `format_version` 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Resource, Reflect)]
#[serde(deny_unknown_fields)]
pub struct XrActionManifest {
    /// The file's format version ([`MANIFEST_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Action sets.
    pub sets: Vec<ActionSetDecl>,
    /// Suggested bindings per interaction profile.
    #[serde(default)]
    pub bindings: Vec<ProfileBindings>,
}

fn invalid(key: impl Into<String>, reason: impl Into<String>) -> XrError {
    XrError::InvalidValue {
        key: key.into(),
        reason: reason.into(),
    }
}

/// Whether `name` is a valid action or set name: `[a-z][a-z0-9_]*`.
pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn action(name: &str, ty: ActionType, subs: &[&str]) -> ActionDecl {
    ActionDecl {
        name: name.to_string(),
        ty,
        label: format!("xr-action-{}", name.replace('_', "-")),
        subs: subs.iter().map(|s| s.to_string()).collect(),
    }
}

fn bind(action: &str, sub: &str, path: &str) -> BindingDecl {
    BindingDecl(action.to_string(), sub.to_string(), path.to_string())
}

impl Default for XrActionManifest {
    /// The default manifest: move (left stick), turn (right stick), select (trigger), grab
    /// (squeeze), primary and secondary buttons, menu, grip and aim poses and a haptic output,
    /// with bindings for the simple controller and a generic two-hand controller profile.
    fn default() -> Self {
        let both = ["left", "right"];
        let gameplay = ActionSetDecl {
            name: "gameplay".into(),
            priority: 0,
            actions: vec![
                action("move", ActionType::Vec2, &["left"]),
                action("turn", ActionType::Vec2, &["right"]),
                action("select", ActionType::Float, &both),
                action("grab", ActionType::Float, &both),
                action("primary", ActionType::Bool, &both),
                action("secondary", ActionType::Bool, &both),
                action("menu", ActionType::Bool, &["left"]),
                action("grip_pose", ActionType::Pose, &both),
                action("aim_pose", ActionType::Pose, &both),
                action("haptic", ActionType::Haptic, &both),
            ],
        };
        let mut simple = Vec::new();
        let mut generic = Vec::new();
        for sub in both {
            simple.push(bind("select", sub, "select/click"));
            simple.push(bind("grip_pose", sub, "grip/pose"));
            simple.push(bind("aim_pose", sub, "aim/pose"));
            simple.push(bind("haptic", sub, &format!("/user/hand/{sub}/output/haptic")));
            generic.push(bind("select", sub, "trigger/value"));
            generic.push(bind("grab", sub, "squeeze/value"));
            generic.push(bind("grip_pose", sub, "grip/pose"));
            generic.push(bind("aim_pose", sub, "aim/pose"));
            generic.push(bind("haptic", sub, &format!("/user/hand/{sub}/output/haptic")));
        }
        simple.push(bind("menu", "left", "menu/click"));
        generic.push(bind("move", "left", "thumbstick"));
        generic.push(bind("turn", "right", "thumbstick"));
        generic.push(bind("primary", "left", "x/click"));
        generic.push(bind("secondary", "left", "y/click"));
        generic.push(bind("primary", "right", "a/click"));
        generic.push(bind("secondary", "right", "b/click"));
        generic.push(bind("menu", "left", "menu/click"));
        XrActionManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            sets: vec![gameplay],
            bindings: vec![
                ProfileBindings {
                    profile: "/interaction_profiles/khr/simple_controller".into(),
                    optional: false,
                    map: simple,
                },
                ProfileBindings {
                    profile: "generic_two_hand_controller".into(),
                    optional: false,
                    map: generic,
                },
            ],
        }
    }
}

impl XrActionManifest {
    /// Parses and validates a manifest. A newer `format_version` is [`XrError::FormatTooNew`].
    pub fn from_ron(text: &str) -> Result<XrActionManifest, XrError> {
        let version = crate::config::read_format_version(text).map_err(|e| invalid("actions.xr.ron", e))?;
        if version > MANIFEST_FORMAT_VERSION {
            return Err(XrError::FormatTooNew {
                what: "actions.xr.ron",
                found: version,
                supported: MANIFEST_FORMAT_VERSION,
            });
        }
        let manifest: XrActionManifest =
            ron::from_str(text).map_err(|e| invalid("actions.xr.ron", e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The canonical RON text.
    pub fn to_ron(&self) -> Result<String, XrError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| invalid("actions.xr.ron", e.to_string()))
    }

    /// Checks names (`[a-z][a-z0-9_]*`, unique per set; set names unique), sub-paths (`left`,
    /// `right`, no repeats), the format version, and that every binding names an existing action
    /// and one of its sub-paths. Errors name the offender.
    pub fn validate(&self) -> Result<(), XrError> {
        if self.format_version != MANIFEST_FORMAT_VERSION {
            return Err(invalid(
                "format_version",
                format!("{} is not {MANIFEST_FORMAT_VERSION}", self.format_version),
            ));
        }
        let mut set_names = BTreeSet::new();
        for set in &self.sets {
            if !valid_name(&set.name) {
                return Err(invalid(format!("set {}", set.name), "names are [a-z][a-z0-9_]*"));
            }
            if !set_names.insert(set.name.as_str()) {
                return Err(invalid(format!("set {}", set.name), "duplicate set name"));
            }
            let mut names = BTreeSet::new();
            for a in &set.actions {
                if !valid_name(&a.name) {
                    return Err(invalid(format!("action {}", a.name), "names are [a-z][a-z0-9_]*"));
                }
                if !names.insert(a.name.as_str()) {
                    return Err(invalid(
                        format!("action {}", a.name),
                        format!("duplicate in set {}", set.name),
                    ));
                }
                let mut subs = BTreeSet::new();
                for sub in &a.subs {
                    if !SUB_PATHS.contains(&sub.as_str()) {
                        return Err(invalid(
                            format!("action {} sub {sub}", a.name),
                            "sub-paths are left or right",
                        ));
                    }
                    if !subs.insert(sub.as_str()) {
                        return Err(invalid(
                            format!("action {} sub {sub}", a.name),
                            "duplicate sub-path",
                        ));
                    }
                }
            }
        }
        for profile in &self.bindings {
            for b in &profile.map {
                let Some(decl) = self.find(b.action()) else {
                    return Err(XrError::ActionUnknown {
                        name: format!("{} (binding in {})", b.action(), profile.profile),
                    });
                };
                if !decl.subs.iter().any(|s| s == b.sub()) && !(decl.subs.is_empty() && b.sub().is_empty()) {
                    return Err(invalid(
                        format!("binding {} {}", b.action(), b.sub()),
                        format!("action {} has no sub-path {}", b.action(), b.sub()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The first action named `name` in any set.
    pub fn find(&self, name: &str) -> Option<&ActionDecl> {
        self.sets
            .iter()
            .flat_map(|s| s.actions.iter())
            .find(|a| a.name == name)
    }

    /// The manifest with `left` and `right` swapped in every binding (a left-handed player).
    pub fn with_dominant_hand(&self, hand: DominantHand) -> XrActionManifest {
        let mut out = self.clone();
        if hand == DominantHand::Left {
            let swap = |s: &str| match s {
                "left" => "right".to_string(),
                "right" => "left".to_string(),
                other => other.to_string(),
            };
            for profile in &mut out.bindings {
                for b in &mut profile.map {
                    let sub = swap(&b.1);
                    let path =
                        b.2.replace("/user/hand/left/", "/user/hand/@/")
                            .replace("/user/hand/right/", "/user/hand/left/")
                            .replace("/user/hand/@/", "/user/hand/right/");
                    *b = BindingDecl(b.0.clone(), sub, path);
                }
            }
            // An action bound to one hand only follows its binding.
            for set in &mut out.sets {
                for a in &mut set.actions {
                    if a.subs.len() == 1 {
                        a.subs[0] = swap(&a.subs[0]);
                    }
                }
            }
        }
        out
    }
}

/// The runtime-side action sets of one session: built from a manifest, frozen by
/// [`XrActionSets::attach`] (exactly once per session).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XrActionSets {
    sets: Vec<ActionSetDecl>,
    attached: bool,
}

impl XrActionSets {
    /// Sets from a (validated) manifest, not attached.
    pub fn from_manifest(manifest: &XrActionManifest) -> Self {
        XrActionSets {
            sets: manifest.sets.clone(),
            attached: false,
        }
    }

    /// Whether the sets are attached.
    pub fn attached(&self) -> bool {
        self.attached
    }

    /// The sets.
    pub fn sets(&self) -> &[ActionSetDecl] {
        &self.sets
    }

    /// Adds an action to `set` before attaching. After [`XrActionSets::attach`] it is
    /// [`XrError::ActionsFrozen`]; an unknown set is [`XrError::ActionUnknown`]; a bad or duplicate
    /// name is [`XrError::InvalidValue`].
    pub fn create_action(&mut self, set: &str, decl: ActionDecl) -> Result<(), XrError> {
        if self.attached {
            return Err(XrError::ActionsFrozen {
                call: "create_action",
            });
        }
        let Some(target) = self.sets.iter_mut().find(|s| s.name == set) else {
            return Err(XrError::ActionUnknown {
                name: format!("set {set}"),
            });
        };
        if !valid_name(&decl.name) || target.actions.iter().any(|a| a.name == decl.name) {
            return Err(invalid(format!("action {}", decl.name), "bad or duplicate name"));
        }
        target.actions.push(decl);
        Ok(())
    }

    /// Attaches the sets to the session. A second call is [`XrError::ActionsFrozen`].
    pub fn attach(&mut self) -> Result<(), XrError> {
        if self.attached {
            return Err(XrError::ActionsFrozen { call: "attach" });
        }
        self.attached = true;
        Ok(())
    }
}

/// One action's value at the last sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum XrActionValue {
    /// A button.
    Bool(bool),
    /// An axis.
    Float(f32),
    /// A stick.
    Vec2(Vec2),
    /// A pose in tracking space.
    Pose(Pose),
}

impl XrActionValue {
    /// The default (released) value of an action of type `ty`; `None` for haptics.
    pub fn default_for(ty: ActionType) -> Option<XrActionValue> {
        match ty {
            ActionType::Bool => Some(XrActionValue::Bool(false)),
            ActionType::Float => Some(XrActionValue::Float(0.0)),
            ActionType::Vec2 => Some(XrActionValue::Vec2(Vec2::ZERO)),
            ActionType::Pose => Some(XrActionValue::Pose(Pose::IDENTITY)),
            ActionType::Haptic => None,
        }
    }

    /// The value as the `Vec3` `regia_input` injection carries (button 0/1, axis x, stick xy).
    /// Poses have none.
    pub fn as_input(&self) -> Option<Vec3> {
        match *self {
            XrActionValue::Bool(b) => Some(Vec3::new(if b { 1.0 } else { 0.0 }, 0.0, 0.0)),
            XrActionValue::Float(f) => Some(Vec3::new(f, 0.0, 0.0)),
            XrActionValue::Vec2(v) => Some(Vec3::new(v.x, v.y, 0.0)),
            XrActionValue::Pose(_) => None,
        }
    }

    /// Whether the value matches type `ty`.
    pub fn matches(&self, ty: ActionType) -> bool {
        matches!(
            (self, ty),
            (XrActionValue::Bool(_), ActionType::Bool)
                | (XrActionValue::Float(_), ActionType::Float)
                | (XrActionValue::Vec2(_), ActionType::Vec2)
                | (XrActionValue::Pose(_), ActionType::Pose)
        )
    }

    /// Whether every numeric component is finite. NaN and infinity are not held.
    pub(crate) fn is_finite(self) -> bool {
        match self {
            XrActionValue::Bool(_) => true,
            XrActionValue::Float(f) => f.is_finite(),
            XrActionValue::Vec2(v) => v.is_finite(),
            XrActionValue::Pose(p) => p.position.is_finite() && p.orientation.is_finite(),
        }
    }
}

/// The built controller field a default-manifest action reads, when its name is one of them.
/// Other names are fed only by injected (scripted) values and published to `regia_input`.
pub fn controller_source(name: &str) -> Option<ControllerAction> {
    Some(match name {
        "select" => ControllerAction::Trigger,
        "grab" => ControllerAction::Squeeze,
        "move" | "turn" => ControllerAction::Thumbstick,
        "primary" => ControllerAction::PrimaryButton,
        "secondary" => ControllerAction::SecondaryButton,
        "thumbstick_click" => ControllerAction::ThumbstickClick,
        "menu" => ControllerAction::Menu,
        _ => return None,
    })
}

fn hand_of(sub: &str) -> Option<Hand> {
    match sub {
        "left" => Some(Hand::Left),
        "right" => Some(Hand::Right),
        _ => None,
    }
}

/// The key of an action's state: `(action, sub-path)`; the sub-path is empty for an action with
/// none.
pub type ActionKey = (String, String);

/// Every action's value at the last sync, and whether it is active. Values change only in
/// [`XrActionStates::sync`]: two reads between syncs are identical.
#[derive(Debug, Clone, Default, PartialEq, Resource)]
pub struct XrActionStates {
    values: BTreeMap<ActionKey, (bool, XrActionValue)>,
    types: BTreeMap<ActionKey, ActionType>,
    injected: BTreeMap<ActionKey, XrActionValue>,
    syncs: u64,
}

impl XrActionStates {
    /// States for every non-haptic action of `sets`, inactive at their defaults.
    pub fn new(sets: &XrActionSets) -> Self {
        let mut out = XrActionStates::default();
        for set in sets.sets() {
            for a in &set.actions {
                let Some(default) = XrActionValue::default_for(a.ty) else {
                    continue;
                };
                let subs: Vec<String> = if a.subs.is_empty() {
                    vec![String::new()]
                } else {
                    a.subs.clone()
                };
                for sub in subs {
                    let key = (a.name.clone(), sub);
                    out.types.insert(key.clone(), a.ty);
                    out.values.insert(key, (false, default));
                }
            }
        }
        out
    }

    /// Syncs performed.
    pub fn syncs(&self) -> u64 {
        self.syncs
    }

    /// Scripts a value for `(action, sub)` (simulator input, `xr.drive input`); it is read at the
    /// next sync and held until replaced. An unknown action, a value of the wrong type, or a
    /// non-finite float, stick or pose is an error. A refused value is not stored.
    pub fn inject(&mut self, action: &str, sub: &str, value: XrActionValue) -> Result<(), XrError> {
        let key = (action.to_string(), sub.to_string());
        let Some(ty) = self.types.get(&key) else {
            return Err(XrError::ActionUnknown {
                name: if sub.is_empty() {
                    action.to_string()
                } else {
                    format!("{action}/{sub}")
                },
            });
        };
        if !value.matches(*ty) {
            return Err(invalid(
                format!("{action}/{sub}"),
                format!("expects a {ty:?} value"),
            ));
        }
        if !value.is_finite() {
            return Err(invalid(format!("{action}/{sub}"), "expects a finite value"));
        }
        self.injected.insert(key, value);
        Ok(())
    }

    /// Reads every action once. `input_live` false (session neither running nor focused): every action is inactive at its
    /// default. Otherwise an injected value wins, then the built controller field of the same
    /// name (pose actions read the grip or aim pose) when the controller is active.
    pub fn sync(&mut self, input_live: bool, controllers: &[ControllerState; 2]) {
        self.syncs += 1;
        for (key, slot) in self.values.iter_mut() {
            let ty = self.types[key];
            let Some(default) = XrActionValue::default_for(ty) else {
                continue;
            };
            if !input_live {
                *slot = (false, default);
                continue;
            }
            if let Some(v) = self.injected.get(key) {
                *slot = (true, *v);
                continue;
            }
            let controller = hand_of(&key.1).map(|h| &controllers[h.index()]);
            let from_controller = controller.filter(|c| c.active).and_then(|c| match ty {
                ActionType::Pose => match key.0.as_str() {
                    "grip_pose" => Some(XrActionValue::Pose(c.grip.pose)),
                    "aim_pose" => Some(XrActionValue::Pose(c.aim.pose)),
                    _ => None,
                },
                _ => controller_source(&key.0).map(|a| match c.action(a) {
                    ActionValue::Bool(b) => XrActionValue::Bool(b),
                    ActionValue::Float(f) => XrActionValue::Float(f),
                    ActionValue::Vec2(v) => XrActionValue::Vec2(v),
                }),
            });
            *slot = match from_controller.filter(|v| v.matches(ty)) {
                Some(v) => (true, v),
                None => (false, default),
            };
        }
    }

    /// `(active, value)` of `(action, sub)` at the last sync.
    pub fn get(&self, action: &str, sub: &str) -> Option<(bool, XrActionValue)> {
        self.values.get(&(action.to_string(), sub.to_string())).copied()
    }

    /// Every `(key, active, value)`, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&ActionKey, bool, XrActionValue)> {
        self.values.iter().map(|(k, (a, v))| (k, *a, *v))
    }

    /// The `regia_input` action name for `(action, sub)`: the action name when it has at most one
    /// sub-path, else `<action>_<sub>` (`grab_left`).
    pub fn input_name(&self, action: &str, sub: &str) -> String {
        let per_hand = self.types.keys().filter(|(a, _)| a == action).count() > 1;
        if per_hand && !sub.is_empty() {
            format!("{action}_{sub}")
        } else {
            action.to_string()
        }
    }

    /// Publishes the synced values to `regia_input`: an active button, axis or stick is held on
    /// its input action ([`XrActionStates::input_name`]); an inactive one is released so device
    /// bindings of the same action keep working. Poses go to `poses`.
    pub fn publish(&self, injection: Option<&mut InputInjection>, poses: &mut ActionPoses) {
        poses.poses.clear();
        let mut held = Vec::new();
        for ((action, sub), (active, value)) in &self.values {
            match value {
                XrActionValue::Pose(p) => {
                    poses.poses.insert((action.clone(), sub.clone()), (*active, *p));
                }
                v => {
                    if let Some(input) = v.as_input() {
                        held.push((self.input_name(action, sub), *active, input));
                    }
                }
            }
        }
        if let Some(injection) = injection {
            for (name, active, input) in held {
                if active {
                    injection.hold(name, input);
                } else {
                    injection.release(&name);
                }
            }
        }
    }
}

/// Pose actions at the last sync, keyed by `(action, sub-path)`: `(active, pose in tracking
/// space)`.
#[derive(Debug, Clone, Default, PartialEq, Resource)]
pub struct ActionPoses {
    /// The poses.
    pub poses: BTreeMap<ActionKey, (bool, Pose)>,
}

impl ActionPoses {
    /// The pose of `(action, sub)` when active.
    pub fn get(&self, action: &str, sub: &str) -> Option<Pose> {
        self.poses
            .get(&(action.to_string(), sub.to_string()))
            .filter(|(active, _)| *active)
            .map(|(_, p)| *p)
    }
}

/// What a haptic request did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HapticOutcome {
    /// Played, with the clamped amplitude.
    Played {
        /// The amplitude sent, 0 to 1.
        amplitude: f32,
    },
    /// The device has no haptics: a soft no-op with a diagnostic, not an error.
    Unsupported,
}

/// Plays a haptic pulse through `backend` for the manifest's haptic `action` on `sub`. The
/// amplitude clamps to 0..=1 (a non-finite one is 0); `frequency_hz` is advisory (the built
/// backend trait has no frequency). An unknown or non-haptic action is
/// [`XrError::ActionUnknown`]; a backend without haptics answers [`HapticOutcome::Unsupported`].
pub fn haptic(
    backend: &mut dyn XrBackend,
    manifest: &XrActionManifest,
    action: &str,
    sub: &str,
    amplitude: f32,
    duration_ns: i64,
    frequency_hz: Option<f32>,
) -> Result<HapticOutcome, XrError> {
    let decl = manifest
        .find(action)
        .filter(|d| d.ty == ActionType::Haptic && d.subs.iter().any(|s| s == sub));
    let (Some(_), Some(hand)) = (decl, hand_of(sub)) else {
        return Err(XrError::ActionUnknown {
            name: format!("{action}/{sub}"),
        });
    };
    let amplitude = if amplitude.is_finite() {
        amplitude.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let duration_ns = duration_ns.max(0);
    let _ = frequency_hz;
    match backend.apply_haptic(hand, amplitude, duration_ns) {
        Ok(()) => Ok(HapticOutcome::Played { amplitude }),
        Err(XrError::Unsupported(_)) => {
            tracing::info!(action, sub, "xr haptic: the device has no haptics (no-op)");
            Ok(HapticOutcome::Unsupported)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_platform::xr::{NullXr, PoseSample, SimEvent};

    const SPEC_EXAMPLE: &str = r#"(format_version: 1,
 sets: [(name: "gameplay", priority: 0, actions: [
   (name: "move", ty: Vec2, label: "xr-action-move", subs: ["left"]),
   (name: "grab", ty: Float, label: "xr-action-grab", subs: ["left","right"]),
   (name: "grip_pose", ty: Pose, subs: ["left","right"]), (name: "buzz", ty: Haptic, subs: ["left","right"])])],
 bindings: [(profile: "simple_controller", map: [("grab", "left", "select/click")])])"#;

    #[test]
    fn the_spec_example_loads_validates_and_round_trips() {
        let m = XrActionManifest::from_ron(SPEC_EXAMPLE).expect("loads");
        assert_eq!(m.sets[0].actions.len(), 4);
        assert_eq!(
            m.bindings[0].map[0].full_path(),
            "/user/hand/left/input/select/click"
        );
        let text = m.to_ron().expect("ser");
        assert_eq!(XrActionManifest::from_ron(&text).expect("reload"), m);
        let d = XrActionManifest::default();
        d.validate().expect("default manifest is valid");
        assert_eq!(
            XrActionManifest::from_ron(&d.to_ron().expect("ser")).expect("reload"),
            d
        );
    }

    #[test]
    fn bad_manifests_are_rejected_with_the_names() {
        let dup = SPEC_EXAMPLE.replace("\"buzz\"", "\"grab\"");
        let e = XrActionManifest::from_ron(&dup).expect_err("dup");
        assert!(
            e.to_string().contains("grab") && e.to_string().contains("duplicate"),
            "{e}"
        );
        let bad_sub = SPEC_EXAMPLE.replace("subs: [\"left\"]", "subs: [\"head\"]");
        let e = XrActionManifest::from_ron(&bad_sub).expect_err("sub");
        assert!(e.to_string().contains("head"), "{e}");
        let bad_name = SPEC_EXAMPLE.replace("\"move\"", "\"Move\"");
        let e = XrActionManifest::from_ron(&bad_name).expect_err("name");
        assert!(e.to_string().contains("Move"), "{e}");
        let unknown = SPEC_EXAMPLE.replace("(\"grab\", \"left\"", "(\"jump\", \"left\"");
        let e = XrActionManifest::from_ron(&unknown).expect_err("unknown");
        assert_eq!(e.code(), "xr_action_unknown");
        assert!(e.to_string().contains("jump"));
        let wrong_sub = SPEC_EXAMPLE.replace("(\"grab\", \"left\"", "(\"move\", \"right\"");
        let e = XrActionManifest::from_ron(&wrong_sub).expect_err("sub");
        assert!(e.to_string().contains("move"), "{e}");
        let v0 = SPEC_EXAMPLE.replace("format_version: 1", "format_version: 0");
        let e = XrActionManifest::from_ron(&v0).expect_err("v0");
        assert!(e.to_string().contains("format_version"), "{e}");
        let v9 = SPEC_EXAMPLE.replace("format_version: 1", "format_version: 9");
        assert_eq!(
            XrActionManifest::from_ron(&v9).expect_err("v9").code(),
            "xr_format_too_new"
        );
    }

    #[test]
    fn attaching_freezes_the_sets() {
        let mut sets = XrActionSets::from_manifest(&XrActionManifest::default());
        let jump = ActionDecl {
            name: "jump".into(),
            ty: ActionType::Bool,
            label: String::new(),
            subs: vec![],
        };
        sets.create_action("gameplay", jump.clone())
            .expect("before attach");
        assert!(sets.create_action("gameplay", jump.clone()).is_err(), "duplicate");
        assert_eq!(
            sets.create_action("ui", jump.clone()).expect_err("no set").code(),
            "xr_action_unknown"
        );
        sets.attach().expect("attach once");
        assert_eq!(sets.attach().expect_err("twice").code(), "xr_actions_frozen");
        let late = ActionDecl {
            name: "late".into(),
            ..jump
        };
        assert_eq!(
            sets.create_action("gameplay", late).expect_err("frozen").code(),
            "xr_actions_frozen"
        );
    }

    fn active_right(trigger: f32) -> [ControllerState; 2] {
        let mut right = ControllerState::inactive(Hand::Right);
        right.active = true;
        right.trigger = trigger;
        right.grip = PoseSample::at_rest(Pose::new(Vec3::new(0.2, 1.0, -0.3), Default::default()), 0);
        [ControllerState::inactive(Hand::Left), right]
    }

    #[test]
    fn unfocused_reads_inactive_and_reads_hold_between_syncs() {
        let sets = XrActionSets::from_manifest(&XrActionManifest::default());
        let mut states = XrActionStates::new(&sets);
        states.sync(false, &active_right(0.8));
        assert_eq!(
            states.get("select", "right"),
            Some((false, XrActionValue::Float(0.0)))
        );
        states.sync(true, &active_right(0.8));
        let first = states.get("select", "right");
        assert_eq!(first, Some((true, XrActionValue::Float(0.8))));
        assert_eq!(
            states.get("select", "right"),
            first,
            "two reads between syncs agree"
        );
        assert_eq!(
            states.get("select", "left"),
            Some((false, XrActionValue::Float(0.0)))
        );
        states
            .inject("menu", "left", XrActionValue::Bool(true))
            .expect("inject");
        assert_eq!(
            states.get("menu", "left"),
            Some((false, XrActionValue::Bool(false))),
            "not until sync"
        );
        states.sync(true, &active_right(0.8));
        assert_eq!(
            states.get("menu", "left"),
            Some((true, XrActionValue::Bool(true)))
        );
        assert!(
            states.inject("menu", "left", XrActionValue::Float(1.0)).is_err(),
            "type"
        );
        assert_eq!(
            states
                .inject("nope", "", XrActionValue::Bool(true))
                .expect_err("unknown")
                .code(),
            "xr_action_unknown"
        );
    }

    #[test]
    fn publishing_holds_active_actions_and_releases_inactive_ones() {
        let sets = XrActionSets::from_manifest(&XrActionManifest::default());
        let mut states = XrActionStates::new(&sets);
        let mut injection = InputInjection::default();
        let mut poses = ActionPoses::default();
        states
            .inject("move", "left", XrActionValue::Vec2(Vec2::new(0.5, -1.0)))
            .expect("inject");
        states.sync(true, &active_right(0.25));
        states.publish(Some(&mut injection), &mut poses);
        assert_eq!(
            injection.value_for("move", 0),
            Some(Vec3::new(0.5, -1.0, 0.0)),
            "one sub: plain name"
        );
        assert_eq!(
            injection.value_for("select_right", 0),
            Some(Vec3::new(0.25, 0.0, 0.0))
        );
        assert_eq!(
            injection.value_for("select_left", 0),
            None,
            "inactive is released"
        );
        assert_eq!(
            poses.get("grip_pose", "right").map(|p| p.position),
            Some(Vec3::new(0.2, 1.0, -0.3))
        );
        assert_eq!(poses.get("grip_pose", "left"), None);
        states.sync(false, &active_right(0.25));
        states.publish(Some(&mut injection), &mut poses);
        assert_eq!(injection.value_for("move", 0), None, "focus loss releases");
    }

    #[test]
    fn haptics_clamp_and_a_missing_device_is_a_soft_no_op() {
        let manifest = XrActionManifest::default();
        let mut xr = NullXr::default();
        xr.simulate(SimEvent::RuntimeReady).expect("ready");
        xr.begin_session().expect("begin");
        let out = haptic(&mut xr, &manifest, "haptic", "left", 3.0, 1000, Some(160.0)).expect("plays");
        assert_eq!(out, HapticOutcome::Played { amplitude: 1.0 });
        let out = haptic(&mut xr, &manifest, "haptic", "right", -1.0, 1000, None).expect("plays");
        assert_eq!(out, HapticOutcome::Played { amplitude: 0.0 });
        assert_eq!(xr.haptics().len(), 2);
        assert_eq!(xr.haptics()[0].amplitude, 1.0);
        assert!(
            haptic(&mut xr, &manifest, "select", "left", 1.0, 1, None).is_err(),
            "not a haptic"
        );

        struct NoHaptics(NullXr);
        impl XrBackend for NoHaptics {
            fn name(&self) -> &'static str {
                "nohaptics"
            }
            fn state(&self) -> regia_platform::xr::SessionState {
                self.0.state()
            }
            fn poll_events(&mut self) -> Vec<regia_platform::xr::XrEvent> {
                self.0.poll_events()
            }
            fn begin_session(&mut self) -> Result<(), XrError> {
                self.0.begin_session()
            }
            fn request_exit(&mut self) -> Result<(), XrError> {
                self.0.request_exit()
            }
            fn end_session(&mut self) -> Result<(), XrError> {
                self.0.end_session()
            }
            fn wait_frame(&mut self) -> Result<regia_platform::xr::FrameTiming, XrError> {
                self.0.wait_frame()
            }
            fn begin_frame(&mut self) -> Result<(), XrError> {
                self.0.begin_frame()
            }
            fn locate_views(&mut self, t: i64) -> Result<[regia_platform::xr::View; 2], XrError> {
                self.0.locate_views(t)
            }
            fn head_sample(&self) -> PoseSample {
                self.0.head_sample()
            }
            fn sync_actions(&mut self) -> Result<(), XrError> {
                self.0.sync_actions()
            }
            fn controller(&self, hand: Hand) -> ControllerState {
                self.0.controller(hand)
            }
            fn apply_haptic(&mut self, _: Hand, _: f32, _: i64) -> Result<(), XrError> {
                Err(XrError::Unsupported("apply_haptic"))
            }
            fn end_frame(&mut self, s: &regia_platform::xr::FrameSubmit) -> Result<(), XrError> {
                self.0.end_frame(s)
            }
        }
        let mut none = NoHaptics(NullXr::default());
        assert_eq!(
            haptic(&mut none, &manifest, "haptic", "left", 0.5, 1, None),
            Ok(HapticOutcome::Unsupported)
        );
    }

    #[test]
    fn a_left_handed_player_swaps_the_bindings() {
        let m = XrActionManifest::default().with_dominant_hand(DominantHand::Left);
        m.validate().expect("still valid");
        assert_eq!(
            m.find("move").map(|a| a.subs.clone()),
            Some(vec!["right".to_string()])
        );
        let generic = &m.bindings[1].map;
        assert!(generic.iter().any(|b| b.action() == "move" && b.sub() == "right"));
        assert!(generic.iter().any(|b| b.action() == "haptic"
            && b.sub() == "left"
            && b.full_path() == "/user/hand/left/output/haptic"));
    }

    #[test]
    fn names_follow_the_pattern() {
        assert!(valid_name("grip_pose2"));
        assert!(!valid_name("2grip"));
        assert!(!valid_name(""));
        assert!(!valid_name("Grip"));
        assert!(!valid_name("grip-pose"));
    }
}
