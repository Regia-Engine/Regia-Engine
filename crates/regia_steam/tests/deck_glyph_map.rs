// SPDX-License-Identifier: MPL-2.0
//! Glyph mapping tables (ADR-1332): the label and texture key of every button in every family,
//! the controller kind of every Steam Input type, USB vendor and common device name, and the
//! prompts a controls screen shows. The tables are written out in full so that any change to a
//! mapping shows up as a diff of this file.

use regia_input::{ActionKind, Binding, InputConfig, InputContext, InputSource, Stick};
use regia_platform::{GamepadAxis, GamepadButton};
use regia_steam::deck_glyphs::{ALL_BUTTONS, ALL_FAMILIES, PromptControl, glyph_set_keys, prompt};
use regia_steam::deck_layout::{LAYOUT_ORDER, control_id, layout_sheet};
use regia_steam::input::UI_ACTIONS;
use regia_steam::{ControllerKind, GlyphFamily, action_prompts};

/// Labels in `ALL_BUTTONS` order: South, East, West, North, LB, RB, LT, RT, LS, RS, D-pad
/// up/down/left/right, Start, Select, Mode.
const LABELS: [(GlyphFamily, [&str; 17]); 4] = [
    (
        GlyphFamily::Xbox,
        [
            "A",
            "B",
            "X",
            "Y",
            "LB",
            "RB",
            "LT",
            "RT",
            "LS",
            "RS",
            "D-pad Up",
            "D-pad Down",
            "D-pad Left",
            "D-pad Right",
            "Menu",
            "View",
            "Guide",
        ],
    ),
    (
        GlyphFamily::PlayStation,
        [
            "Cross",
            "Circle",
            "Square",
            "Triangle",
            "L1",
            "R1",
            "L2",
            "R2",
            "L3",
            "R3",
            "D-pad Up",
            "D-pad Down",
            "D-pad Left",
            "D-pad Right",
            "Options",
            "Create",
            "PS",
        ],
    ),
    (
        GlyphFamily::Switch,
        [
            "B",
            "A",
            "Y",
            "X",
            "L",
            "R",
            "ZL",
            "ZR",
            "L Stick Press",
            "R Stick Press",
            "D-pad Up",
            "D-pad Down",
            "D-pad Left",
            "D-pad Right",
            "+",
            "-",
            "Home",
        ],
    ),
    (
        GlyphFamily::SteamDeck,
        [
            "A",
            "B",
            "X",
            "Y",
            "LB",
            "RB",
            "LT",
            "RT",
            "LS",
            "RS",
            "D-pad Up",
            "D-pad Down",
            "D-pad Left",
            "D-pad Right",
            "Menu",
            "View",
            "Steam",
        ],
    ),
];

#[test]
fn every_button_label_in_every_family() {
    assert_eq!(LABELS.len(), ALL_FAMILIES.len());
    for (family, labels) in LABELS {
        for (button, expected) in ALL_BUTTONS.iter().zip(labels) {
            assert_eq!(family.label(*button), expected, "{family:?} {button:?}");
            let p = prompt(family, PromptControl::Button(*button));
            assert_eq!(p.label, expected);
        }
    }
}

#[test]
fn texture_keys_are_distinct_and_derived_from_the_label() {
    for family in ALL_FAMILIES {
        let mut keys: Vec<String> = ALL_BUTTONS.iter().map(|b| family.texture_key(*b)).collect();
        for key in &keys {
            let stem = key
                .strip_prefix(&format!("glyphs/{}/", family.dir()))
                .unwrap_or_else(|| panic!("{key} is under the family dir"));
            assert!(!stem.is_empty());
            assert!(
                stem.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
                "{key}: file-system and URL safe"
            );
        }
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            ALL_BUTTONS.len(),
            "{family:?}: one texture per button"
        );
        // The set is the buttons plus both sticks and the D-pad.
        let set = glyph_set_keys(family);
        assert_eq!(set.len(), ALL_BUTTONS.len() + 3);
        for key in &keys {
            assert!(set.contains(key), "{key}");
        }
    }
    assert_eq!(
        GlyphFamily::Switch.texture_key(GamepadButton::Start),
        "glyphs/switch/plus"
    );
    assert_eq!(
        GlyphFamily::Switch.texture_key(GamepadButton::Select),
        "glyphs/switch/minus"
    );
    assert_eq!(
        GlyphFamily::Switch.texture_key(GamepadButton::LeftStick),
        "glyphs/switch/l_stick_press"
    );
    assert_eq!(
        GlyphFamily::Xbox.texture_key(GamepadButton::DPadUp),
        "glyphs/xbox/d-pad_up"
    );
    assert_eq!(
        GlyphFamily::SteamDeck.texture_key(GamepadButton::Mode),
        "glyphs/deck/steam"
    );
}

#[test]
fn sticks_triggers_and_the_dpad_share_glyphs_across_axes() {
    for family in ALL_FAMILIES {
        let dir = family.dir();
        let key = |control| prompt(family, control).texture_key;
        assert_eq!(
            key(PromptControl::Axis(GamepadAxis::LeftStickX)),
            format!("glyphs/{dir}/left_stick")
        );
        assert_eq!(
            key(PromptControl::Axis(GamepadAxis::LeftStickY)),
            format!("glyphs/{dir}/left_stick")
        );
        assert_eq!(
            key(PromptControl::Stick(Stick::Right)),
            format!("glyphs/{dir}/right_stick")
        );
        assert_eq!(
            key(PromptControl::Axis(GamepadAxis::DPadX)),
            format!("glyphs/{dir}/dpad")
        );
        assert_eq!(
            key(PromptControl::Axis(GamepadAxis::DPadY)),
            format!("glyphs/{dir}/dpad")
        );
        assert_eq!(
            key(PromptControl::Axis(GamepadAxis::RightTrigger)),
            family.texture_key(GamepadButton::RightTrigger)
        );
        // A stick click is a button, not the stick.
        assert_ne!(
            key(PromptControl::Button(GamepadButton::LeftStick)),
            key(PromptControl::Stick(Stick::Left))
        );
    }
}

#[test]
fn steam_input_types_map_to_kinds_and_families() {
    // ESteamInputType numbers (public Steamworks documentation).
    let table = [
        (0, ControllerKind::Unknown, GlyphFamily::Xbox),
        (1, ControllerKind::SteamController, GlyphFamily::SteamDeck),
        (2, ControllerKind::Xbox, GlyphFamily::Xbox),
        (3, ControllerKind::Xbox, GlyphFamily::Xbox),
        (4, ControllerKind::Generic, GlyphFamily::Xbox),
        (5, ControllerKind::PlayStation, GlyphFamily::PlayStation),
        (6, ControllerKind::Unknown, GlyphFamily::Xbox),
        (7, ControllerKind::Unknown, GlyphFamily::Xbox),
        (8, ControllerKind::Switch, GlyphFamily::Switch),
        (9, ControllerKind::Switch, GlyphFamily::Switch),
        (10, ControllerKind::Switch, GlyphFamily::Switch),
        (11, ControllerKind::Unknown, GlyphFamily::Xbox),
        (12, ControllerKind::PlayStation, GlyphFamily::PlayStation),
        (13, ControllerKind::PlayStation, GlyphFamily::PlayStation),
        (14, ControllerKind::SteamDeck, GlyphFamily::SteamDeck),
        (15, ControllerKind::Unknown, GlyphFamily::Xbox),
    ];
    for (number, kind, family) in table {
        assert_eq!(
            ControllerKind::from_steam_input_type(number),
            kind,
            "type {number}"
        );
        assert_eq!(kind.glyphs(), family, "{kind:?}");
    }
}

#[test]
fn usb_vendors_map_to_kinds() {
    let table = [
        (0x045e, ControllerKind::Xbox),
        (0x054c, ControllerKind::PlayStation),
        (0x057e, ControllerKind::Switch),
        (0x28de, ControllerKind::SteamDeck),
        (0x2dc8, ControllerKind::Generic), // 8BitDo
        (0x046d, ControllerKind::Generic), // Logitech
    ];
    for (vendor, kind) in table {
        assert_eq!(ControllerKind::from_vendor_id(vendor), kind, "{vendor:#06x}");
    }
}

#[test]
fn device_names_map_to_kinds_off_and_on_a_deck() {
    // (name, kind off a Deck, kind on a Deck)
    let table = [
        (
            "Xbox Wireless Controller",
            ControllerKind::Xbox,
            ControllerKind::SteamDeck,
        ),
        (
            "Microsoft X-Box 360 pad",
            ControllerKind::Xbox,
            ControllerKind::SteamDeck,
        ),
        (
            "Microsoft X-Box One S pad",
            ControllerKind::Xbox,
            ControllerKind::SteamDeck,
        ),
        (
            "Xbox Series X Controller",
            ControllerKind::Xbox,
            ControllerKind::SteamDeck,
        ),
        (
            "Steam Virtual Gamepad",
            ControllerKind::Generic,
            ControllerKind::SteamDeck,
        ),
        (
            "Valve Software Steam Deck Controller",
            ControllerKind::SteamDeck,
            ControllerKind::SteamDeck,
        ),
        (
            "Steam Controller",
            ControllerKind::SteamController,
            ControllerKind::SteamController,
        ),
        (
            "Sony Interactive Entertainment DualSense Wireless Controller",
            ControllerKind::PlayStation,
            ControllerKind::PlayStation,
        ),
        (
            "DualSense Edge Wireless Controller",
            ControllerKind::PlayStation,
            ControllerKind::PlayStation,
        ),
        (
            "Sony Interactive Entertainment Wireless Controller",
            ControllerKind::PlayStation,
            ControllerKind::PlayStation,
        ),
        (
            "PS4 Controller",
            ControllerKind::PlayStation,
            ControllerKind::PlayStation,
        ),
        (
            "PS5 Controller",
            ControllerKind::PlayStation,
            ControllerKind::PlayStation,
        ),
        (
            "PS3 Controller",
            ControllerKind::PlayStation,
            ControllerKind::PlayStation,
        ),
        (
            "Nintendo Switch Pro Controller",
            ControllerKind::Switch,
            ControllerKind::Switch,
        ),
        ("Pro Controller", ControllerKind::Switch, ControllerKind::Switch),
        (
            "Nintendo Switch Joy-Con (L/R)",
            ControllerKind::Switch,
            ControllerKind::Switch,
        ),
        ("Joy-Con (R)", ControllerKind::Switch, ControllerKind::Switch),
        ("8BitDo Pro 2", ControllerKind::Generic, ControllerKind::SteamDeck),
        (
            "Logitech Gamepad F310",
            ControllerKind::Generic,
            ControllerKind::SteamDeck,
        ),
        ("", ControllerKind::Unknown, ControllerKind::Unknown),
        ("   ", ControllerKind::Unknown, ControllerKind::Unknown),
    ];
    for (name, off_deck, on_deck) in table {
        assert_eq!(ControllerKind::from_device_name(name), off_deck, "{name:?}");
        assert_eq!(
            ControllerKind::for_connected_pad(name, false),
            off_deck,
            "{name:?} off a Deck"
        );
        assert_eq!(
            ControllerKind::for_connected_pad(name, true),
            on_deck,
            "{name:?} on a Deck"
        );
    }
}

#[test]
fn the_ui_accept_and_cancel_prompts_are_positional() {
    // The UI confirms with the south button and cancels with the east one on every pad (the
    // positional convention of `UI_ACTIONS`); the label shown is what is printed there.
    let accept = UI_ACTIONS
        .iter()
        .find(|(name, _)| *name == "ui_accept")
        .map(|(_, b)| *b);
    let cancel = UI_ACTIONS
        .iter()
        .find(|(name, _)| *name == "ui_cancel")
        .map(|(_, b)| *b);
    assert_eq!(accept, Some(GamepadButton::South));
    assert_eq!(cancel, Some(GamepadButton::East));
    let expected = [
        (GlyphFamily::Xbox, "A", "B"),
        (GlyphFamily::PlayStation, "Cross", "Circle"),
        (GlyphFamily::Switch, "B", "A"),
        (GlyphFamily::SteamDeck, "A", "B"),
    ];
    for (family, ok, back) in expected {
        assert_eq!(family.label(GamepadButton::South), ok, "{family:?}");
        assert_eq!(family.label(GamepadButton::East), back, "{family:?}");
    }
}

fn racing() -> InputConfig {
    InputConfig::new()
        .action("Steer", ActionKind::Axis1D)
        .action("Throttle", ActionKind::Axis1D)
        .action("Brake", ActionKind::Axis1D)
        .action("Camera", ActionKind::Axis2D)
        .action("Horn", ActionKind::Button)
        .context("Driving", InputContext::new(0))
        .binding_in(
            "Driving",
            Binding::new("Steer", InputSource::gamepad_axis(GamepadAxis::LeftStickX)),
        )
        .binding_in(
            "Driving",
            Binding::new("Steer", InputSource::gamepad_axis(GamepadAxis::DPadX)),
        )
        .binding_in(
            "Driving",
            Binding::new("Throttle", InputSource::gamepad_axis(GamepadAxis::RightTrigger)),
        )
        .binding_in(
            "Driving",
            Binding::new("Throttle", InputSource::gamepad_button(GamepadButton::South)),
        )
        .binding_in(
            "Driving",
            Binding::new("Brake", InputSource::gamepad_axis(GamepadAxis::LeftTrigger)),
        )
        .binding_in(
            "Driving",
            Binding::new("Camera", InputSource::gamepad_stick(Stick::Right)),
        )
        .binding_in(
            "Driving",
            Binding::new("Horn", InputSource::gamepad_button(GamepadButton::LeftStick)),
        )
        .binding_in("Driving", Binding::new("Horn", InputSource::key("H")))
}

#[test]
fn action_prompts_per_family_for_a_racing_layout() {
    let config = racing();
    let labels = |action: &str, family| -> Vec<String> {
        action_prompts(&config, "Driving", action, family)
            .into_iter()
            .map(|p| p.label)
            .collect()
    };
    assert_eq!(labels("Steer", GlyphFamily::Xbox), ["Left Stick", "D-pad"]);
    assert_eq!(labels("Throttle", GlyphFamily::PlayStation), ["R2", "Cross"]);
    assert_eq!(labels("Throttle", GlyphFamily::Switch), ["ZR", "B"]);
    assert_eq!(labels("Brake", GlyphFamily::SteamDeck), ["LT"]);
    assert_eq!(labels("Camera", GlyphFamily::Switch), ["Right Stick"]);
    assert_eq!(
        labels("Horn", GlyphFamily::PlayStation),
        ["L3"],
        "the key binding is skipped"
    );
    assert_eq!(labels("Horn", GlyphFamily::Switch), ["L Stick Press"]);
}

#[test]
fn the_controls_sheet_for_every_family() {
    let config = racing();
    let expected: [(GlyphFamily, [&str; 6]); 4] = [
        (
            GlyphFamily::Xbox,
            ["A", "LT", "RT", "Left Stick", "LS", "Right Stick"],
        ),
        (
            GlyphFamily::PlayStation,
            ["Cross", "L2", "R2", "Left Stick", "L3", "Right Stick"],
        ),
        (
            GlyphFamily::Switch,
            ["B", "ZL", "ZR", "Left Stick", "L Stick Press", "Right Stick"],
        ),
        (
            GlyphFamily::SteamDeck,
            ["A", "LT", "RT", "Left Stick", "LS", "Right Stick"],
        ),
    ];
    for (family, head) in expected {
        let sheet = layout_sheet(&config, "Driving", family).expect("Driving exists");
        let labels: Vec<&str> = sheet.rows.iter().map(|r| r.prompt.label.as_str()).collect();
        // The six rows above, then the D-pad (Steer).
        assert_eq!(labels.len(), 7, "{family:?}: {labels:?}");
        assert_eq!(&labels[..6], head, "{family:?}");
        assert_eq!(labels[6], "D-pad");
        assert_eq!(sheet.rows[6].actions, ["Steer"]);
        assert!(sheet.keyboard_only.is_empty(), "Horn has L3");
    }
    // Every control id the sheet can name is in the layout order.
    let ids: std::collections::HashSet<&str> = LAYOUT_ORDER.iter().map(|c| control_id(*c)).collect();
    assert_eq!(ids.len(), 20, "control ids are distinct");
}
