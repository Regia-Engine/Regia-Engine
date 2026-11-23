// SPDX-License-Identifier: MPL-2.0
//! The `InputGlyph` table (`docs/specs/11` § 7, ADR-7070): which glyph a button prompt shows for an
//! action on a device family, as an asset a project ships (`assets://input/project.glyphs.ron`,
//! `docs/formats/input-glyphs-ron.md`).
//!
//! A lookup ([`InputGlyphs::lookup`]) tries, in order:
//!
//! 1. the table: `glyphs[action][device]`, then `glyphs[action]["gamepad"]` for the three pad
//!    families ([`GlyphSource::Table`]);
//! 2. the built-in families for the UI hint actions (`activate`, `back`, ... — [`HintDevice::glyph`],
//!    [`GlyphSource::BuiltIn`]);
//! 3. the action's first binding in the [`regia_input::InputConfig`] that the device family can
//!    press: a key or mouse button for keyboard and mouse, a pad source for the pads, named the way
//!    that family prints it (`RT` / `R2`, `A` / `Cross`) ([`GlyphSource::Binding`]).
//!
//! Tooltip placeholders (`{activate}`, `{Fire}`) go through the same lookup ([`expand_glyphs`]).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_input::{DeviceClass, InputConfig, InputSource, Stick};
use regia_platform::{GamepadAxis, GamepadButton};
use serde::{Deserialize, Serialize};

use crate::UiError;
use crate::hints::{HintAction, HintDevice};

/// The newest `.glyphs.ron` version this build reads.
pub const INPUT_GLYPHS_VERSION: u32 = 1;

/// Where a project's table lives (loaded at start-up when `UiPlugin::load_project_assets` is on).
pub const INPUT_GLYPHS_PATH: &str = "assets://input/project.glyphs.ron";

/// The key a table row uses for every pad family at once.
pub const ANY_GAMEPAD: &str = "gamepad";

/// Longest `.glyphs.ron` text parsed, in bytes (a table is a few kilobytes).
const MAX_TABLE_BYTES: usize = 1 << 20;

/// The schema hash input of `.glyphs.ron` (`cargo xtask check-formats`).
#[must_use]
pub fn input_glyphs_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<InputGlyphTable>()])
}

/// Migration steps registered for `.glyphs.ron`: none, v1 is the first version.
#[must_use]
pub fn input_glyphs_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// A `.glyphs.ron` file: per action, per device family name (`keyboard_mouse`, `xbox`,
/// `playstation`, `steam_deck`, or `gamepad` for every pad), the glyph text (a label or an
/// `<icon=name>` markup span).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputGlyphTable {
    /// Format version ([`INPUT_GLYPHS_VERSION`]).
    pub version: u32,
    /// Action name → device family name → glyph.
    #[serde(default)]
    pub glyphs: BTreeMap<String, BTreeMap<String, String>>,
}

impl InputGlyphTable {
    /// Parses `.glyphs.ron` text. A version newer than [`INPUT_GLYPHS_VERSION`], an unknown device
    /// name or an empty glyph is an error naming the row.
    pub fn from_ron(text: &str) -> Result<Self, UiError> {
        let invalid = |reason: String| UiError::Invalid {
            path: INPUT_GLYPHS_PATH.to_owned(),
            reason,
        };
        if text.len() > MAX_TABLE_BYTES {
            return Err(invalid(format!(
                "input_glyphs_too_large: {} bytes (limit {MAX_TABLE_BYTES})",
                text.len()
            )));
        }
        let table: Self = ron::from_str(text).map_err(|e| invalid(format!("input_glyphs_parse: {e}")))?;
        if table.version == 0 || table.version > INPUT_GLYPHS_VERSION {
            return Err(invalid(format!(
                "input_glyphs_version: version {} (this build reads 1..={INPUT_GLYPHS_VERSION})",
                table.version
            )));
        }
        for (action, row) in &table.glyphs {
            for (device, glyph) in row {
                if device != ANY_GAMEPAD && HintDevice::parse(device).is_none() {
                    return Err(invalid(format!(
                        "input_glyphs_device: {action:?} names device {device:?} (keyboard_mouse, xbox, playstation, steam_deck or gamepad)"
                    )));
                }
                if glyph.trim().is_empty() {
                    return Err(invalid(format!(
                        "input_glyphs_empty: {action:?} has an empty glyph for {device:?}"
                    )));
                }
            }
        }
        Ok(table)
    }

    /// The table as `.glyphs.ron` text.
    pub fn to_ron(&self) -> Result<String, UiError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| UiError::Invalid {
            path: INPUT_GLYPHS_PATH.to_owned(),
            reason: format!("input_glyphs_write: {e}"),
        })
    }
}

/// Where a glyph came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSource {
    /// The project's table.
    Table,
    /// The built-in family table of the UI hint actions.
    BuiltIn,
    /// The action's binding in the input config.
    Binding,
}

impl GlyphSource {
    /// Stable lowercase name (agent protocol).
    pub fn name(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::BuiltIn => "builtin",
            Self::Binding => "binding",
        }
    }
}

/// One resolved glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphLookup {
    /// The text to show.
    pub glyph: String,
    /// Which step of the lookup produced it.
    pub source: GlyphSource,
}

/// The project's glyph table as a resource (empty: every lookup falls back).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct InputGlyphs(pub InputGlyphTable);

impl InputGlyphs {
    /// Wraps a parsed table.
    pub fn new(table: InputGlyphTable) -> Self {
        Self(table)
    }

    /// The glyph `device` shows for `action` (see the module docs for the order), `None` when no
    /// step knows one.
    pub fn lookup(
        &self,
        action: &str,
        device: HintDevice,
        config: Option<&InputConfig>,
    ) -> Option<GlyphLookup> {
        if let Some(row) = self.0.glyphs.get(action) {
            let pad = device != HintDevice::KeyboardMouse;
            let hit = row
                .get(device.name())
                .or_else(|| pad.then(|| row.get(ANY_GAMEPAD)).flatten());
            if let Some(glyph) = hit {
                return Some(GlyphLookup {
                    glyph: glyph.clone(),
                    source: GlyphSource::Table,
                });
            }
        }
        if let Some(hint) = HintAction::parse(action) {
            return Some(GlyphLookup {
                glyph: device.glyph(hint).to_owned(),
                source: GlyphSource::BuiltIn,
            });
        }
        let config = config?;
        let wants_pad = device != HintDevice::KeyboardMouse;
        let mut contexts: Vec<_> = config.contexts.iter().collect();
        // Highest priority first, then by name: the binding a player most likely uses.
        contexts.sort_by(|a, b| b.1.priority.cmp(&a.1.priority).then_with(|| a.0.cmp(b.0)));
        contexts
            .into_iter()
            .flat_map(|(_, c)| c.bindings.iter())
            .filter(|b| b.action == action)
            .find(|b| (b.input.class() == DeviceClass::Gamepad) == wants_pad)
            .map(|b| GlyphLookup {
                glyph: source_glyph(&b.input, device),
                source: GlyphSource::Binding,
            })
    }
}

/// How `device` names one input source: keys and mouse buttons by the rebinding label, pad
/// buttons and axes by the family's printed names.
pub fn source_glyph(source: &InputSource, device: HintDevice) -> String {
    match source {
        InputSource::GamepadButton(button) => pad_button_glyph(*button, device).to_owned(),
        InputSource::GamepadStick(Stick::Left) => stick_glyph(device, true).to_owned(),
        InputSource::GamepadStick(Stick::Right) => stick_glyph(device, false).to_owned(),
        InputSource::GamepadAxis(axis) => match axis {
            GamepadAxis::LeftStickX | GamepadAxis::LeftStickY => stick_glyph(device, true).to_owned(),
            GamepadAxis::RightStickX | GamepadAxis::RightStickY => stick_glyph(device, false).to_owned(),
            GamepadAxis::LeftTrigger => pad_button_glyph(GamepadButton::LeftTrigger, device).to_owned(),
            GamepadAxis::RightTrigger => pad_button_glyph(GamepadButton::RightTrigger, device).to_owned(),
            _ => regia_input::source_label(source),
        },
        _ => regia_input::source_label(source),
    }
}

fn stick_glyph(device: HintDevice, left: bool) -> &'static str {
    match (device, left) {
        (HintDevice::PlayStation, true) => "L",
        (HintDevice::PlayStation, false) => "R",
        (_, true) => "LS",
        (_, false) => "RS",
    }
}

/// A pad button as the family prints it.
pub fn pad_button_glyph(button: GamepadButton, device: HintDevice) -> &'static str {
    use GamepadButton as B;
    let ps = device == HintDevice::PlayStation;
    let deck = device == HintDevice::SteamDeck;
    match button {
        B::South if ps => "Cross",
        B::East if ps => "Circle",
        B::West if ps => "Square",
        B::North if ps => "Triangle",
        B::South => "A",
        B::East => "B",
        B::West => "X",
        B::North => "Y",
        B::LeftShoulder if ps || deck => "L1",
        B::RightShoulder if ps || deck => "R1",
        B::LeftTrigger if ps || deck => "L2",
        B::RightTrigger if ps || deck => "R2",
        B::LeftShoulder => "LB",
        B::RightShoulder => "RB",
        B::LeftTrigger => "LT",
        B::RightTrigger => "RT",
        B::LeftStick if ps => "L3",
        B::RightStick if ps => "R3",
        B::LeftStick => "LS",
        B::RightStick => "RS",
        B::DPadUp => "D-pad Up",
        B::DPadDown => "D-pad Down",
        B::DPadLeft => "D-pad Left",
        B::DPadRight => "D-pad Right",
        B::Start if ps => "Options",
        B::Select if ps => "Share",
        B::Start => "Menu",
        B::Select => "View",
        B::Mode if ps => "PS",
        B::Mode if deck => "Steam",
        B::Mode => "Guide",
    }
}

/// Replaces `{name}` placeholders with the glyph `device` shows for the action `name` (table, then
/// built-in, then binding); placeholders no step knows and unmatched braces stay as written.
pub fn expand_glyphs(
    text: &str,
    device: HintDevice,
    glyphs: &InputGlyphs,
    config: Option<&InputConfig>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        match tail.find('}') {
            Some(close) => {
                let name = &tail[1..close];
                match glyphs.lookup(name, device, config) {
                    Some(hit) => out.push_str(&hit.glyph),
                    None => out.push_str(&tail[..=close]),
                }
                rest = &tail[close + 1..];
            }
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `Startup` (with `UiPlugin::load_project_assets`): reads [`INPUT_GLYPHS_PATH`] into
/// [`InputGlyphs`]. A missing file keeps the empty table; a bad one is logged
/// (`input_glyphs_invalid`) and ignored.
pub fn load_input_glyphs(world: &mut World) {
    let Some(vfs) = world.get_resource::<regia_platform::Vfs>().cloned() else {
        return;
    };
    let Ok(path) = regia_platform::VfsPath::parse(INPUT_GLYPHS_PATH) else {
        return;
    };
    let Ok(bytes) = vfs.read_sync(&path) else {
        return;
    };
    match InputGlyphTable::from_ron(&String::from_utf8_lossy(&bytes)) {
        Ok(table) => {
            world.insert_resource(InputGlyphs(table));
        }
        Err(e) => tracing::warn!(target: "regia_ui", code = "input_glyphs_invalid", "{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_round_trips_and_rejects_bad_rows() {
        let text = r#"(version: 1, glyphs: { "Jump": { "gamepad": "A", "keyboard_mouse": "Space" } })"#;
        let table = InputGlyphTable::from_ron(text).expect("parses");
        let again = InputGlyphTable::from_ron(&table.to_ron().expect("writes")).expect("reparses");
        assert_eq!(table, again);
        assert!(InputGlyphTable::from_ron(r#"(version: 1, glyphs: { "Jump": { "switch": "B" } })"#).is_err());
        assert!(InputGlyphTable::from_ron(r#"(version: 1, glyphs: { "Jump": { "xbox": " " } })"#).is_err());
        assert!(InputGlyphTable::from_ron("(version: 0)").is_err());
        assert!(InputGlyphTable::from_ron("not ron").is_err());
    }

    #[test]
    fn the_gamepad_row_covers_every_pad_family_but_not_the_keyboard() {
        let glyphs = InputGlyphs::new(
            InputGlyphTable::from_ron(r#"(version: 1, glyphs: { "Jump": { "gamepad": "Ⓐ" } })"#).unwrap(),
        );
        for device in [HintDevice::Xbox, HintDevice::PlayStation, HintDevice::SteamDeck] {
            assert_eq!(glyphs.lookup("Jump", device, None).unwrap().glyph, "Ⓐ");
        }
        assert!(glyphs.lookup("Jump", HintDevice::KeyboardMouse, None).is_none());
    }

    #[test]
    fn placeholders_expand_through_the_table() {
        let glyphs = InputGlyphs::new(
            InputGlyphTable::from_ron(r#"(version: 1, glyphs: { "Fire": { "xbox": "RT" } })"#).unwrap(),
        );
        assert_eq!(
            expand_glyphs(
                "{Fire} shoots, {back} leaves, {nope}",
                HintDevice::Xbox,
                &glyphs,
                None
            ),
            "RT shoots, B leaves, {nope}"
        );
    }

    #[test]
    fn pad_names_follow_the_family() {
        assert_eq!(
            pad_button_glyph(GamepadButton::South, HintDevice::PlayStation),
            "Cross"
        );
        assert_eq!(
            pad_button_glyph(GamepadButton::RightTrigger, HintDevice::Xbox),
            "RT"
        );
        assert_eq!(
            pad_button_glyph(GamepadButton::RightTrigger, HintDevice::SteamDeck),
            "R2"
        );
    }
}
