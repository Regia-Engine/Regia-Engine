// SPDX-License-Identifier: MPL-2.0
//! Controller prompt glyph sets (ADR-526): which glyph a game draws for an *action*, and whether a
//! game ships a complete glyph set for every controller family.
//!
//! ADR-368 (`deck_prep`) picks the [`GlyphFamily`] from the connected pad and labels single
//! buttons. This module goes from a Regia Engine action to its prompts: the gamepad bindings of the
//! action in a context become one [`Prompt`] each (button, stick or axis), labeled and keyed for the
//! active family. [`glyph_set_keys`] lists every texture a family needs, and [`missing_glyphs`]
//! checks a game's asset folder for them, so a missing Deck or PlayStation glyph is found before
//! a player sees an Xbox letter on a Deck.
//!
//! Texture keys follow `GlyphFamily::texture_key`: `glyphs/<family>/<name>`. Sticks are
//! `left_stick` / `right_stick`, and both D-pad axes share `dpad`.

use std::path::Path;

use regia_input::{DeviceClass, InputConfig, InputSource, Stick};
use regia_platform::{GamepadAxis, GamepadButton};

use crate::deck_prep::GlyphFamily;

/// Every button of the standard layout, in a stable order (the order of `GamepadButton`).
pub const ALL_BUTTONS: [GamepadButton; 17] = [
    GamepadButton::South,
    GamepadButton::East,
    GamepadButton::West,
    GamepadButton::North,
    GamepadButton::LeftShoulder,
    GamepadButton::RightShoulder,
    GamepadButton::LeftTrigger,
    GamepadButton::RightTrigger,
    GamepadButton::LeftStick,
    GamepadButton::RightStick,
    GamepadButton::DPadUp,
    GamepadButton::DPadDown,
    GamepadButton::DPadLeft,
    GamepadButton::DPadRight,
    GamepadButton::Start,
    GamepadButton::Select,
    GamepadButton::Mode,
];

/// Every family, in a stable order.
pub const ALL_FAMILIES: [GlyphFamily; 4] = [
    GlyphFamily::Xbox,
    GlyphFamily::PlayStation,
    GlyphFamily::Switch,
    GlyphFamily::SteamDeck,
];

/// The gamepad control a prompt points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptControl {
    /// A button (a stick click is `LeftStick` / `RightStick`).
    Button(GamepadButton),
    /// Moving a whole stick.
    Stick(Stick),
    /// One axis: a stick axis, an analog trigger or a D-pad axis.
    Axis(GamepadAxis),
}

impl PromptControl {
    /// The control a gamepad input source points at; `None` for keyboard and mouse sources.
    pub fn from_source(source: &InputSource) -> Option<Self> {
        match source {
            InputSource::GamepadButton(button) => Some(Self::Button(*button)),
            InputSource::GamepadStick(stick) => Some(Self::Stick(*stick)),
            InputSource::GamepadAxis(axis) => Some(Self::Axis(*axis)),
            _ => None,
        }
    }

    /// The control a prompt draws: a stick axis draws the whole stick, a trigger axis draws the
    /// trigger button, so `LeftStickX` and `LeftStickY` share one glyph. Both D-pad axes are the
    /// one D-pad control (`Axis(DPadX)`), so an action on both axes gets one D-pad prompt.
    pub fn canonical(self) -> Self {
        match self {
            Self::Axis(GamepadAxis::LeftStickX | GamepadAxis::LeftStickY) => Self::Stick(Stick::Left),
            Self::Axis(GamepadAxis::RightStickX | GamepadAxis::RightStickY) => Self::Stick(Stick::Right),
            Self::Axis(GamepadAxis::LeftTrigger) => Self::Button(GamepadButton::LeftTrigger),
            Self::Axis(GamepadAxis::RightTrigger) => Self::Button(GamepadButton::RightTrigger),
            Self::Axis(GamepadAxis::DPadX | GamepadAxis::DPadY) => Self::Axis(GamepadAxis::DPadX),
            other => other,
        }
    }
}

/// One prompt glyph: what to draw for a control in a family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// The control (already [`PromptControl::canonical`]).
    pub control: PromptControl,
    /// The text label, the fallback when the texture is missing.
    pub label: String,
    /// The texture key, `glyphs/<family>/<name>`.
    pub texture_key: String,
}

/// The label of `control` in `family`.
pub fn control_label(family: GlyphFamily, control: PromptControl) -> String {
    match control.canonical() {
        PromptControl::Button(button) => family.label(button).to_owned(),
        PromptControl::Stick(Stick::Left) => "Left Stick".to_owned(),
        PromptControl::Stick(Stick::Right) => "Right Stick".to_owned(),
        PromptControl::Axis(_) => "D-pad".to_owned(),
    }
}

/// The texture key of `control` in `family`.
pub fn control_texture_key(family: GlyphFamily, control: PromptControl) -> String {
    match control.canonical() {
        PromptControl::Button(button) => family.texture_key(button),
        PromptControl::Stick(Stick::Left) => format!("glyphs/{}/left_stick", family.dir()),
        PromptControl::Stick(Stick::Right) => format!("glyphs/{}/right_stick", family.dir()),
        PromptControl::Axis(_) => format!("glyphs/{}/dpad", family.dir()),
    }
}

/// The prompt for `control` in `family`.
pub fn prompt(family: GlyphFamily, control: PromptControl) -> Prompt {
    let control = control.canonical();
    Prompt {
        control,
        label: control_label(family, control),
        texture_key: control_texture_key(family, control),
    }
}

/// The family whose texture directory is `name` (`xbox`, `playstation`, `switch`, `deck`).
pub fn family_from_dir(name: &str) -> Option<GlyphFamily> {
    ALL_FAMILIES.into_iter().find(|family| family.dir() == name)
}

/// The prompts for `action`'s gamepad bindings in `context`, in binding order, one per distinct
/// control. Keyboard and mouse bindings are skipped; an unknown context or action yields none.
pub fn action_prompts(config: &InputConfig, context: &str, action: &str, family: GlyphFamily) -> Vec<Prompt> {
    let Some(context) = config.contexts.get(context) else {
        return Vec::new();
    };
    let mut prompts: Vec<Prompt> = Vec::new();
    for binding in context
        .bindings
        .iter()
        .filter(|b| b.action == action && b.input.class() == DeviceClass::Gamepad)
    {
        if let Some(control) = PromptControl::from_source(&binding.input) {
            let next = prompt(family, control);
            if !prompts.iter().any(|p| p.control == next.control) {
                prompts.push(next);
            }
        }
    }
    prompts
}

/// Every texture key a complete glyph set of `family` has: the 17 buttons, both sticks and the
/// D-pad, sorted and without duplicates.
pub fn glyph_set_keys(family: GlyphFamily) -> Vec<String> {
    let mut keys: Vec<String> = ALL_BUTTONS
        .iter()
        .map(|&b| control_texture_key(family, PromptControl::Button(b)))
        .chain([
            control_texture_key(family, PromptControl::Stick(Stick::Left)),
            control_texture_key(family, PromptControl::Stick(Stick::Right)),
            control_texture_key(family, PromptControl::Axis(GamepadAxis::DPadX)),
        ])
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// The texture file extensions [`missing_glyphs`] accepts by default: source images and the
/// cooked texture container.
pub const GLYPH_EXTENSIONS: [&str; 3] = ["png", "ktx2", "webp"];

/// The keys of `family`'s set with no file under `asset_root`: a key `glyphs/deck/a` is present
/// when `<asset_root>/glyphs/deck/a.<ext>` exists for one of `extensions`. Sorted. An unreadable
/// root reports every key missing (nothing panics).
pub fn missing_glyphs(asset_root: &Path, family: GlyphFamily, extensions: &[&str]) -> Vec<String> {
    glyph_set_keys(family)
        .into_iter()
        .filter(|key| {
            !extensions
                .iter()
                .any(|ext| asset_root.join(format!("{key}.{ext}")).is_file())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_input::{ActionKind, Binding, InputContext};

    fn config() -> InputConfig {
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("Move", ActionKind::Axis2D)
            .action("Throttle", ActionKind::Axis1D)
            .action("Steer", ActionKind::Axis1D)
            .context("Gameplay", InputContext::new(0))
            .binding_in("Gameplay", Binding::new("Jump", InputSource::key("Space")))
            .binding_in(
                "Gameplay",
                Binding::new("Jump", InputSource::gamepad_button(GamepadButton::South)),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Jump", InputSource::gamepad_button(GamepadButton::South)),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Jump", InputSource::gamepad_button(GamepadButton::North)),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Move", InputSource::gamepad_stick(Stick::Left)),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Throttle", InputSource::gamepad_axis(GamepadAxis::RightTrigger)),
            )
            .binding_in(
                "Gameplay",
                Binding::new("Steer", InputSource::gamepad_axis(GamepadAxis::LeftStickX)),
            )
    }

    #[test]
    fn prompts_follow_the_gamepad_bindings_in_order_without_duplicates() {
        let prompts = action_prompts(&config(), "Gameplay", "Jump", GlyphFamily::PlayStation);
        let labels: Vec<&str> = prompts.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Cross", "Triangle"],
            "key binding skipped, duplicate South merged"
        );
        assert_eq!(prompts[0].texture_key, "glyphs/playstation/cross");
        let deck = action_prompts(&config(), "Gameplay", "Jump", GlyphFamily::SteamDeck);
        assert_eq!(deck[0].texture_key, "glyphs/deck/a");
    }

    #[test]
    fn axes_draw_their_stick_or_trigger() {
        let throttle = action_prompts(&config(), "Gameplay", "Throttle", GlyphFamily::Switch);
        assert_eq!(throttle.len(), 1);
        assert_eq!(throttle[0].label, "ZR");
        assert_eq!(
            throttle[0].control,
            PromptControl::Button(GamepadButton::RightTrigger)
        );
        let steer = action_prompts(&config(), "Gameplay", "Steer", GlyphFamily::Xbox);
        assert_eq!(steer[0].control, PromptControl::Stick(Stick::Left));
        assert_eq!(steer[0].texture_key, "glyphs/xbox/left_stick");
        let moving = action_prompts(&config(), "Gameplay", "Move", GlyphFamily::Xbox);
        assert_eq!(
            moving[0], steer[0],
            "an axis of a stick and the stick share one glyph"
        );
        assert_eq!(
            prompt(GlyphFamily::Xbox, PromptControl::Axis(GamepadAxis::DPadY)).texture_key,
            "glyphs/xbox/dpad"
        );
    }

    #[test]
    fn an_action_on_both_dpad_axes_gets_one_dpad_prompt() {
        let config = InputConfig::new()
            .action("Pan", ActionKind::Axis2D)
            .context("Map", InputContext::new(0))
            .binding_in(
                "Map",
                Binding::new("Pan", InputSource::gamepad_axis(GamepadAxis::DPadX)),
            )
            .binding_in(
                "Map",
                Binding::new("Pan", InputSource::gamepad_axis(GamepadAxis::DPadY)),
            );
        let prompts = action_prompts(&config, "Map", "Pan", GlyphFamily::PlayStation);
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].label, "D-pad");
        assert_eq!(prompts[0].texture_key, "glyphs/playstation/dpad");
        assert_eq!(prompts[0].control, PromptControl::Axis(GamepadAxis::DPadX));
    }

    #[test]
    fn unknown_context_or_action_gives_no_prompt() {
        assert!(action_prompts(&config(), "Menu", "Jump", GlyphFamily::Xbox).is_empty());
        assert!(action_prompts(&config(), "Gameplay", "Fly", GlyphFamily::Xbox).is_empty());
    }

    #[test]
    fn every_family_has_a_full_distinct_key_set() {
        for family in ALL_FAMILIES {
            let keys = glyph_set_keys(family);
            // 17 buttons + 2 sticks + the D-pad, none colliding after lower-casing.
            assert_eq!(keys.len(), 20, "{family:?}: {keys:?}");
            assert!(
                keys.iter()
                    .all(|k| k.starts_with(&format!("glyphs/{}/", family.dir())))
            );
            assert!(keys.iter().all(|k| {
                k.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '/' || c == '-')
            }));
            assert_eq!(family_from_dir(family.dir()), Some(family));
        }
        assert_eq!(family_from_dir("gamecube"), None);
    }

    #[test]
    fn missing_glyphs_reads_the_asset_folder() {
        let root = regia_test_utils::unique_temp_dir("deck_glyph_set");
        let family = GlyphFamily::SteamDeck;
        let keys = glyph_set_keys(family);
        assert_eq!(missing_glyphs(&root, family, &GLYPH_EXTENSIONS), keys);
        for (i, key) in keys.iter().enumerate() {
            let path = root.join(format!("{key}.{}", GLYPH_EXTENSIONS[i % GLYPH_EXTENSIONS.len()]));
            std::fs::create_dir_all(path.parent().expect("key has a directory")).expect("mkdir");
            std::fs::write(&path, b"glyph").expect("write glyph");
        }
        assert!(missing_glyphs(&root, family, &GLYPH_EXTENSIONS).is_empty());
        std::fs::remove_file(root.join(format!("{}.png", keys[0]))).expect("remove one");
        assert_eq!(
            missing_glyphs(&root, family, &GLYPH_EXTENSIONS),
            vec![keys[0].clone()]
        );
        assert_eq!(
            missing_glyphs(&root, family, &["tga"]).len(),
            keys.len(),
            "only the listed extensions count"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
