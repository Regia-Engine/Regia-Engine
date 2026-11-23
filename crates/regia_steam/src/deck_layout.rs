// SPDX-License-Identifier: MPL-2.0
//! Controller layouts (ADR-1332): the controls screen of an input context for a glyph family, and a
//! check that a game's input config is playable on a controller alone.
//!
//! [`layout_sheet`] turns the gamepad bindings of one context into the rows a "Controls" screen
//! shows: one row per physical control in a fixed order ([`LAYOUT_ORDER`]: face buttons,
//! shoulders and triggers, sticks, D-pad, menu buttons), each with the family's prompt and the
//! actions on it. The same sheet prints as text ([`LayoutSheet::to_text`]) for `regia steam
//! layout` and the book.
//!
//! [`check_layout`] is the controller half of Valve's Deck Verified review, done on the config
//! instead of by hand. It reports, per context:
//! - `no_gamepad_binding` (error): an action bound to keys or the mouse but to no gamepad control,
//!   so a Deck player cannot do it (unless the game lists it in [`LayoutCheck::keyboard_only_ok`]);
//! - `reserved_control` (warning): a binding on the Steam / Guide / PS / Home button, which the
//!   system menu takes (on the Deck the game never sees it), so it does not count as a binding;
//! - `control_conflict` (warning): two actions on one control with plain triggers (no hold, tap,
//!   pulse or chord to tell them apart), so one press fires both;
//! - `missing_glyphs` (warning, only with [`LayoutCheck::glyph_root`]): a family whose glyph
//!   textures are incomplete under the game's asset folder.
//!
//! The standard layout is the 17 buttons of `GamepadButton`, two sticks and the D-pad. The Deck's
//! back grips (L4, R4, L5, R5) and trackpads have no `GamepadButton`: a player binds them to the
//! game's actions in Steam Input's configurator, through the in-game actions file
//! (`crate::input`).

use std::path::PathBuf;

use regia_input::{Binding, DeviceClass, InputConfig, InputSource, Stick, Trigger};
use regia_platform::{GamepadAxis, GamepadButton};

use crate::deck_glyphs::{
    ALL_FAMILIES, GLYPH_EXTENSIONS, Prompt, PromptControl, glyph_set_keys, missing_glyphs, prompt,
};
use crate::deck_prep::GlyphFamily;

/// The order a controls screen lists controls in: face buttons (south, east, west, north),
/// shoulders, triggers, each stick before its click, the D-pad buttons and the D-pad as a whole,
/// then the menu buttons. Every [`PromptControl::canonical`] control is listed exactly once.
pub const LAYOUT_ORDER: [PromptControl; 20] = [
    PromptControl::Button(GamepadButton::South),
    PromptControl::Button(GamepadButton::East),
    PromptControl::Button(GamepadButton::West),
    PromptControl::Button(GamepadButton::North),
    PromptControl::Button(GamepadButton::LeftShoulder),
    PromptControl::Button(GamepadButton::RightShoulder),
    PromptControl::Button(GamepadButton::LeftTrigger),
    PromptControl::Button(GamepadButton::RightTrigger),
    PromptControl::Stick(Stick::Left),
    PromptControl::Button(GamepadButton::LeftStick),
    PromptControl::Stick(Stick::Right),
    PromptControl::Button(GamepadButton::RightStick),
    PromptControl::Button(GamepadButton::DPadUp),
    PromptControl::Button(GamepadButton::DPadDown),
    PromptControl::Button(GamepadButton::DPadLeft),
    PromptControl::Button(GamepadButton::DPadRight),
    PromptControl::Axis(GamepadAxis::DPadX),
    PromptControl::Button(GamepadButton::Start),
    PromptControl::Button(GamepadButton::Select),
    PromptControl::Button(GamepadButton::Mode),
];

/// The control the system menu owns: the Steam button on the Deck, Guide on Xbox pads, PS and
/// Home. A binding on it never counts as the action's gamepad binding.
pub const RESERVED_CONTROL: PromptControl = PromptControl::Button(GamepadButton::Mode);

/// The family-neutral id of a control (snake case): `south`, `left_trigger`, `left_stick`,
/// `left_stick_click`, `dpad_up`, `dpad`, `start`, `mode`, … Used in issues and the agent result.
pub fn control_id(control: PromptControl) -> &'static str {
    match control.canonical() {
        PromptControl::Button(button) => match button {
            GamepadButton::South => "south",
            GamepadButton::East => "east",
            GamepadButton::West => "west",
            GamepadButton::North => "north",
            GamepadButton::LeftShoulder => "left_shoulder",
            GamepadButton::RightShoulder => "right_shoulder",
            GamepadButton::LeftTrigger => "left_trigger",
            GamepadButton::RightTrigger => "right_trigger",
            GamepadButton::LeftStick => "left_stick_click",
            GamepadButton::RightStick => "right_stick_click",
            GamepadButton::DPadUp => "dpad_up",
            GamepadButton::DPadDown => "dpad_down",
            GamepadButton::DPadLeft => "dpad_left",
            GamepadButton::DPadRight => "dpad_right",
            GamepadButton::Start => "start",
            GamepadButton::Select => "select",
            GamepadButton::Mode => "mode",
        },
        PromptControl::Stick(Stick::Left) => "left_stick",
        PromptControl::Stick(Stick::Right) => "right_stick",
        PromptControl::Axis(_) => "dpad",
    }
}

/// The position of `control` (canonicalized) in [`LAYOUT_ORDER`].
pub fn layout_rank(control: PromptControl) -> usize {
    let control = control.canonical();
    LAYOUT_ORDER
        .iter()
        .position(|c| *c == control)
        .unwrap_or(LAYOUT_ORDER.len())
}

/// One row of a controls screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutRow {
    /// The control's prompt in the sheet's family.
    pub prompt: Prompt,
    /// The actions on the control, in binding order, without duplicates.
    pub actions: Vec<String>,
}

/// What a controls screen shows for one input context and one glyph family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutSheet {
    /// The glyph family the prompts are for.
    pub family: GlyphFamily,
    /// The input context.
    pub context: String,
    /// The bound controls, in [`LAYOUT_ORDER`].
    pub rows: Vec<LayoutRow>,
    /// Actions this context binds to keys or the mouse but to no usable gamepad control (the
    /// reserved control does not count), sorted.
    pub keyboard_only: Vec<String>,
}

impl LayoutSheet {
    /// The sheet as text: a header line, then one line per row with the label padded to the
    /// widest one and the actions joined by `, `, then the keyboard-only actions when there are
    /// any. Deterministic, so tests and docs can compare it.
    pub fn to_text(&self) -> String {
        let width = self
            .rows
            .iter()
            .map(|row| row.prompt.label.chars().count())
            .max()
            .unwrap_or(0);
        let mut out = format!("{} ({})\n", self.context, self.family.dir());
        for row in &self.rows {
            let label = &row.prompt.label;
            let pad = width - label.chars().count();
            out.push_str(&format!(
                "  {label}{}  {}\n",
                " ".repeat(pad),
                row.actions.join(", ")
            ));
        }
        if !self.keyboard_only.is_empty() {
            out.push_str(&format!(
                "  keyboard or mouse only: {}\n",
                self.keyboard_only.join(", ")
            ));
        }
        out
    }
}

/// The gamepad control of a binding, canonicalized; `None` for keyboard and mouse bindings.
fn gamepad_control(binding: &Binding) -> Option<PromptControl> {
    if binding.input.class() != DeviceClass::Gamepad {
        return None;
    }
    PromptControl::from_source(&binding.input).map(PromptControl::canonical)
}

/// The controls sheet of `context` for `family`; `None` when the config has no such context.
pub fn layout_sheet(config: &InputConfig, context: &str, family: GlyphFamily) -> Option<LayoutSheet> {
    let bindings = &config.contexts.get(context)?.bindings;
    let mut rows: Vec<LayoutRow> = Vec::new();
    for binding in bindings {
        let Some(control) = gamepad_control(binding) else {
            continue;
        };
        match rows.iter_mut().find(|row| row.prompt.control == control) {
            Some(row) => {
                if !row.actions.contains(&binding.action) {
                    row.actions.push(binding.action.clone());
                }
            }
            None => rows.push(LayoutRow {
                prompt: prompt(family, control),
                actions: vec![binding.action.clone()],
            }),
        }
    }
    rows.sort_by_key(|row| layout_rank(row.prompt.control));
    Some(LayoutSheet {
        family,
        context: context.to_owned(),
        rows,
        keyboard_only: keyboard_only_actions(bindings),
    })
}

/// The sheets of every context for `family`, highest priority first, then by name.
pub fn layout_sheets(config: &InputConfig, family: GlyphFamily) -> Vec<LayoutSheet> {
    let mut names: Vec<(&String, i32)> = config
        .contexts
        .iter()
        .map(|(name, context)| (name, context.priority))
        .collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    names
        .into_iter()
        .filter_map(|(name, _)| layout_sheet(config, name, family))
        .collect()
}

/// The actions of `bindings` with a key or mouse binding and no gamepad binding other than the
/// reserved control, sorted.
fn keyboard_only_actions(bindings: &[Binding]) -> Vec<String> {
    let mut actions: Vec<String> = bindings
        .iter()
        .filter(|b| b.input.class() != DeviceClass::Gamepad)
        .map(|b| b.action.clone())
        .filter(|action| {
            !bindings.iter().any(|b| {
                b.action == *action && gamepad_control(b).is_some_and(|control| control != RESERVED_CONTROL)
            })
        })
        .collect();
    actions.sort();
    actions.dedup();
    actions
}

/// How bad a [`LayoutIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayoutSeverity {
    /// Something a controller player cannot do.
    Error,
    /// Something that works but is likely a mistake.
    Warning,
}

impl LayoutSeverity {
    /// `error` or `warning`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Issue code: an action has key or mouse bindings and no usable gamepad binding.
pub const NO_GAMEPAD_BINDING: &str = "no_gamepad_binding";
/// Issue code: a binding on the system menu button.
pub const RESERVED_CONTROL_CODE: &str = "reserved_control";
/// Issue code: two actions share one control with plain triggers.
pub const CONTROL_CONFLICT: &str = "control_conflict";
/// Issue code: a glyph family is incomplete under the asset folder.
pub const MISSING_GLYPHS: &str = "missing_glyphs";

/// Every issue code [`check_layout`] reports.
pub const LAYOUT_ISSUE_CODES: [&str; 4] = [
    NO_GAMEPAD_BINDING,
    RESERVED_CONTROL_CODE,
    CONTROL_CONFLICT,
    MISSING_GLYPHS,
];

/// One finding of [`check_layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutIssue {
    /// Error or warning.
    pub severity: LayoutSeverity,
    /// One of [`LAYOUT_ISSUE_CODES`].
    pub code: &'static str,
    /// The input context, when the issue is about one.
    pub context: Option<String>,
    /// The action (for a conflict, the actions joined by `, `).
    pub action: Option<String>,
    /// The control's [`control_id`], when the issue is about one.
    pub control: Option<&'static str>,
    /// A sentence for the developer.
    pub message: String,
}

/// What [`check_layout`] checks besides the bindings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutCheck {
    /// Actions that may have no gamepad binding (a debug console key, a screenshot key).
    pub keyboard_only_ok: Vec<String>,
    /// The game's asset folder; every family's glyph set is checked under it. `None` skips the
    /// glyph check.
    pub glyph_root: Option<PathBuf>,
}

/// Whether a binding fires on every press with nothing to tell it from another plain binding on
/// the same control (no trigger, or only down / pressed / released).
fn plain_triggers(binding: &Binding) -> bool {
    binding
        .triggers
        .iter()
        .all(|t| matches!(t, Trigger::Down | Trigger::Pressed | Trigger::Released))
}

/// Checks `config` for a controller-only player (see the module docs for the codes). Issues are
/// sorted by severity, context, action, code and control, so the output is stable.
pub fn check_layout(config: &InputConfig, check: &LayoutCheck) -> Vec<LayoutIssue> {
    let mut issues = Vec::new();
    for (name, context) in &config.contexts {
        let bindings = &context.bindings;
        for action in keyboard_only_actions(bindings) {
            if check.keyboard_only_ok.contains(&action) {
                continue;
            }
            issues.push(LayoutIssue {
                severity: LayoutSeverity::Error,
                code: NO_GAMEPAD_BINDING,
                context: Some(name.clone()),
                message: format!(
                    "`{action}` in `{name}` has key or mouse bindings but no gamepad binding: a Steam Deck player cannot do it"
                ),
                action: Some(action),
                control: None,
            });
        }
        let mut reserved: Vec<&str> = Vec::new();
        for binding in bindings
            .iter()
            .filter(|b| gamepad_control(b) == Some(RESERVED_CONTROL))
        {
            if !reserved.contains(&binding.action.as_str()) {
                reserved.push(&binding.action);
            }
        }
        for action in reserved {
            issues.push(LayoutIssue {
                severity: LayoutSeverity::Warning,
                code: RESERVED_CONTROL_CODE,
                context: Some(name.clone()),
                action: Some(action.to_owned()),
                control: Some(control_id(RESERVED_CONTROL)),
                message: format!(
                    "`{action}` in `{name}` is on the system button (Steam on the Deck, Guide, PS, Home): the system menu takes it, so it is not a usable binding"
                ),
            });
        }
        for control in LAYOUT_ORDER {
            if control == RESERVED_CONTROL {
                continue;
            }
            let mut actions: Vec<&str> = Vec::new();
            for binding in bindings
                .iter()
                .filter(|b| gamepad_control(b) == Some(control) && plain_triggers(b))
            {
                if !actions.contains(&binding.action.as_str()) {
                    actions.push(&binding.action);
                }
            }
            if actions.len() > 1 {
                let joined = actions.join(", ");
                issues.push(LayoutIssue {
                    severity: LayoutSeverity::Warning,
                    code: CONTROL_CONFLICT,
                    context: Some(name.clone()),
                    control: Some(control_id(control)),
                    message: format!(
                        "{joined} share `{}` in `{name}` with no hold, tap, pulse or chord to tell them apart: one press fires all of them",
                        control_id(control)
                    ),
                    action: Some(joined),
                });
            }
        }
    }
    if let Some(root) = &check.glyph_root {
        for family in ALL_FAMILIES {
            let missing = missing_glyphs(root, family, &GLYPH_EXTENSIONS);
            if missing.is_empty() {
                continue;
            }
            let total = glyph_set_keys(family).len();
            issues.push(LayoutIssue {
                severity: LayoutSeverity::Warning,
                code: MISSING_GLYPHS,
                context: None,
                action: None,
                control: None,
                message: format!(
                    "{} of {total} `{}` glyphs are missing under {} (prompts fall back to text labels): {}",
                    missing.len(),
                    family.dir(),
                    root.display(),
                    missing.join(", ")
                ),
            });
        }
    }
    issues.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.context.cmp(&b.context))
            .then_with(|| a.action.cmp(&b.action))
            .then_with(|| a.code.cmp(b.code))
            .then_with(|| a.control.cmp(&b.control))
    });
    issues
}

/// Parses a `config/input.ron` text (`InputConfig::from_ron`), for tools that do not depend on
/// `regia_input` themselves (`regia steam layout`).
pub fn input_config_from_ron(ron: &str) -> Result<InputConfig, String> {
    InputConfig::from_ron(ron)
}

/// Whether any issue is an error.
pub fn has_errors(issues: &[LayoutIssue]) -> bool {
    issues.iter().any(|i| i.severity == LayoutSeverity::Error)
}

/// The gamepad source a [`LayoutRow`] stands for, for tools that rebind from the sheet: a
/// button, a stick or the D-pad X axis.
pub fn row_source(row: &LayoutRow) -> InputSource {
    match row.prompt.control {
        PromptControl::Button(button) => InputSource::GamepadButton(button),
        PromptControl::Stick(stick) => InputSource::GamepadStick(stick),
        PromptControl::Axis(axis) => InputSource::GamepadAxis(axis),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_input::{ActionKind, InputContext};

    fn pad(button: GamepadButton) -> InputSource {
        InputSource::gamepad_button(button)
    }

    /// A small shooter: gameplay, a menu context and a debug key.
    fn config() -> InputConfig {
        InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .action("Jump", ActionKind::Button)
            .action("Fire", ActionKind::Button)
            .action("Aim", ActionKind::Button)
            .action("Reload", ActionKind::Button)
            .action("Interact", ActionKind::Button)
            .action("Map", ActionKind::Button)
            .action("Pause", ActionKind::Button)
            .action("Console", ActionKind::Button)
            .action("Accept", ActionKind::Button)
            .action("Back", ActionKind::Button)
            .context("Gameplay", InputContext::new(0))
            .binding_in(
                "Gameplay",
                Binding::new("Move", InputSource::keys2d("W", "S", "A", "D")),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Move", InputSource::gamepad_stick(Stick::Left)),
            )
            .binding_in("Gameplay", Binding::new("Look", InputSource::MouseDelta))
            .binding_in(
                "Gameplay",
                Binding::new("Look", InputSource::gamepad_axis(GamepadAxis::RightStickX)),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Look", InputSource::gamepad_axis(GamepadAxis::RightStickY)),
            )
            .binding_in("Gameplay", Binding::new("Jump", InputSource::key("Space")))
            .binding_in("Gameplay", Binding::new("Jump", pad(GamepadButton::South)))
            .binding_in(
                "Gameplay",
                Binding::new("Fire", InputSource::gamepad_axis(GamepadAxis::RightTrigger)),
            )
            .binding_in("Gameplay", Binding::new("Aim", pad(GamepadButton::LeftTrigger)))
            .binding_in("Gameplay", Binding::new("Reload", pad(GamepadButton::West)))
            // Interact shares West, but only on a hold: not a conflict.
            .binding_in(
                "Gameplay",
                Binding::new("Interact", pad(GamepadButton::West)).with_trigger(Trigger::Hold(0.4)),
            )
            .binding_in("Gameplay", Binding::new("Map", InputSource::key("M")))
            .binding_in("Gameplay", Binding::new("Map", pad(GamepadButton::Mode)))
            .binding_in("Gameplay", Binding::new("Pause", pad(GamepadButton::Start)))
            .binding_in("Gameplay", Binding::new("Console", InputSource::key("Backquote")))
            .context("Menu", InputContext::consuming(10))
            .binding_in("Menu", Binding::new("Accept", pad(GamepadButton::South)))
            .binding_in("Menu", Binding::new("Back", pad(GamepadButton::East)))
            .binding_in("Menu", Binding::new("Back", InputSource::key("Escape")))
    }

    #[test]
    fn layout_order_lists_every_canonical_control_once() {
        let mut seen = std::collections::HashSet::new();
        for control in LAYOUT_ORDER {
            assert_eq!(control.canonical(), control, "{control:?} is canonical");
            assert!(seen.insert(control), "{control:?} listed twice");
        }
        let ids: std::collections::HashSet<&str> = LAYOUT_ORDER.iter().map(|c| control_id(*c)).collect();
        assert_eq!(ids.len(), LAYOUT_ORDER.len(), "control ids are distinct");
        for id in ids {
            assert!(id.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{id}");
        }
        // Every axis lands on a listed control.
        for axis in [
            GamepadAxis::LeftStickX,
            GamepadAxis::LeftStickY,
            GamepadAxis::RightStickX,
            GamepadAxis::RightStickY,
            GamepadAxis::LeftTrigger,
            GamepadAxis::RightTrigger,
            GamepadAxis::DPadX,
            GamepadAxis::DPadY,
        ] {
            assert!(
                layout_rank(PromptControl::Axis(axis)) < LAYOUT_ORDER.len(),
                "{axis:?}"
            );
        }
    }

    #[test]
    fn the_sheet_lists_bound_controls_in_layout_order() {
        let sheet = layout_sheet(&config(), "Gameplay", GlyphFamily::SteamDeck).expect("context");
        let rows: Vec<(&str, Vec<&str>)> = sheet
            .rows
            .iter()
            .map(|r| {
                (
                    r.prompt.label.as_str(),
                    r.actions.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("A", vec!["Jump"]),
                ("X", vec!["Reload", "Interact"]),
                ("LT", vec!["Aim"]),
                ("RT", vec!["Fire"]),
                ("Left Stick", vec!["Move"]),
                ("Right Stick", vec!["Look"]),
                ("Menu", vec!["Pause"]),
                ("Steam", vec!["Map"]),
            ]
        );
        assert_eq!(sheet.keyboard_only, vec!["Console".to_owned(), "Map".to_owned()]);
        assert_eq!(sheet.rows[0].prompt.texture_key, "glyphs/deck/a");
    }

    #[test]
    fn the_sheet_follows_the_family() {
        let switch = layout_sheet(&config(), "Gameplay", GlyphFamily::Switch).expect("context");
        let labels: Vec<&str> = switch.rows.iter().map(|r| r.prompt.label.as_str()).collect();
        // Positional mapping: the south button is B on a Nintendo pad.
        assert_eq!(
            labels,
            ["B", "Y", "ZL", "ZR", "Left Stick", "Right Stick", "+", "Home"]
        );
        let ps = layout_sheet(&config(), "Menu", GlyphFamily::PlayStation).expect("context");
        let labels: Vec<&str> = ps.rows.iter().map(|r| r.prompt.label.as_str()).collect();
        assert_eq!(labels, ["Cross", "Circle"]);
        assert!(ps.keyboard_only.is_empty());
        assert!(layout_sheet(&config(), "Driving", GlyphFamily::Xbox).is_none());
    }

    #[test]
    fn sheets_come_highest_priority_first_and_print_aligned() {
        let sheets = layout_sheets(&config(), GlyphFamily::Xbox);
        let names: Vec<&str> = sheets.iter().map(|s| s.context.as_str()).collect();
        assert_eq!(names, ["Menu", "Gameplay"]);
        assert_eq!(sheets[0].to_text(), "Menu (xbox)\n  A  Accept\n  B  Back\n");
        let text = sheets[1].to_text();
        assert!(
            text.starts_with("Gameplay (xbox)\n  A            Jump\n"),
            "{text}"
        );
        assert!(text.contains("  Right Stick  Look\n"), "{text}");
        assert!(
            text.ends_with("  keyboard or mouse only: Console, Map\n"),
            "{text}"
        );
    }

    #[test]
    fn the_check_finds_unplayable_reserved_and_conflicting_bindings() {
        let mut config = config();
        // Two plain actions on Start: a conflict.
        config
            .contexts
            .get_mut("Gameplay")
            .expect("gameplay")
            .bindings
            .push(Binding::new("Map", pad(GamepadButton::Start)));
        let issues = check_layout(&config, &LayoutCheck::default());
        let found: Vec<(&str, &str, Option<&str>, Option<&str>)> = issues
            .iter()
            .map(|i| (i.severity.name(), i.code, i.action.as_deref(), i.control))
            .collect();
        assert_eq!(
            found,
            vec![
                ("error", NO_GAMEPAD_BINDING, Some("Console"), None),
                ("warning", RESERVED_CONTROL_CODE, Some("Map"), Some("mode")),
                ("warning", CONTROL_CONFLICT, Some("Pause, Map"), Some("start")),
            ],
            "{issues:#?}"
        );
        assert!(has_errors(&issues));
        // Map now has Start, so it is no longer keyboard-only; Reload/Interact on West are told
        // apart by the hold and are not a conflict.
        assert!(issues.iter().all(|i| i.control != Some("west")));
    }

    #[test]
    fn allowed_keyboard_actions_pass() {
        let check = LayoutCheck {
            keyboard_only_ok: vec!["Console".into(), "Map".into()],
            glyph_root: None,
        };
        let issues = check_layout(&config(), &check);
        assert!(!has_errors(&issues), "{issues:#?}");
        assert_eq!(issues.len(), 1, "only the reserved-button warning");
        assert_eq!(issues[0].code, RESERVED_CONTROL_CODE);
        for issue in &issues {
            assert!(LAYOUT_ISSUE_CODES.contains(&issue.code));
        }
    }

    #[test]
    fn the_glyph_check_reports_incomplete_families_only() {
        let root = regia_test_utils::unique_temp_dir("deck_layout_glyphs");
        for family in ALL_FAMILIES {
            if family == GlyphFamily::Switch {
                continue;
            }
            for key in glyph_set_keys(family) {
                let path = root.join(format!("{key}.png"));
                std::fs::create_dir_all(path.parent().expect("key dir")).expect("mkdir");
                std::fs::write(&path, b"glyph").expect("write");
            }
        }
        let check = LayoutCheck {
            keyboard_only_ok: vec!["Console".into(), "Map".into()],
            glyph_root: Some(root.clone()),
        };
        let issues = check_layout(&config(), &check);
        let glyphs: Vec<&LayoutIssue> = issues.iter().filter(|i| i.code == MISSING_GLYPHS).collect();
        assert_eq!(glyphs.len(), 1, "{issues:#?}");
        assert!(
            glyphs[0].message.starts_with("20 of 20 `switch` glyphs"),
            "{}",
            glyphs[0].message
        );
        assert_eq!(glyphs[0].severity, LayoutSeverity::Warning);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_row_maps_back_to_its_source() {
        let sheet = layout_sheet(&config(), "Gameplay", GlyphFamily::Xbox).expect("context");
        let sources: Vec<InputSource> = sheet.rows.iter().map(row_source).collect();
        assert_eq!(sources[0], pad(GamepadButton::South));
        assert!(sources.contains(&InputSource::gamepad_stick(Stick::Right)));
        assert!(sources.contains(&pad(GamepadButton::RightTrigger)));
    }
}
