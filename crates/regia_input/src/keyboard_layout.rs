// SPDX-License-Identifier: MPL-2.0
//! Explicit key-label presets. Device events and saved bindings always use physical codes.
//!
//! Layout facts: <https://colemak.com/> and the French layout in
//! <https://www.w3.org/TR/uievents-code/>. Original Regia implementation (ADR-8034).

use crate::{InputSource, KeyCode};

/// Labels for the 30 keys in the main letter block, independent of the OS text layout.
///
/// Use a preset when constructing bindings or displaying their labels. Do not translate
/// [`regia_platform::KeyboardInput::physical_key`] through it: those events already name
/// physical positions. No preset changes existing bindings or user overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyboardLayout {
    /// US QWERTY labels.
    Qwerty,
    /// Original Colemak labels (not Colemak-DH).
    Colemak,
    /// Classic French AZERTY labels (not Belgian or the revised French standard).
    Azerty,
}

const PHYSICAL: [&str; 30] = [
    "KeyQ",
    "KeyW",
    "KeyE",
    "KeyR",
    "KeyT",
    "KeyY",
    "KeyU",
    "KeyI",
    "KeyO",
    "KeyP",
    "KeyA",
    "KeyS",
    "KeyD",
    "KeyF",
    "KeyG",
    "KeyH",
    "KeyJ",
    "KeyK",
    "KeyL",
    "Semicolon",
    "KeyZ",
    "KeyX",
    "KeyC",
    "KeyV",
    "KeyB",
    "KeyN",
    "KeyM",
    "Comma",
    "Period",
    "Slash",
];

impl KeyboardLayout {
    /// Supported presets in stable display order.
    pub const ALL: [Self; 3] = [Self::Qwerty, Self::Colemak, Self::Azerty];

    /// Stable lowercase preset name for host settings and the agent protocol.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Qwerty => "qwerty",
            Self::Colemak => "colemak",
            Self::Azerty => "azerty",
        }
    }

    /// Parses a preset name, ignoring ASCII case. Unknown names return `None`.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|layout| layout.name().eq_ignore_ascii_case(name))
    }

    /// `(label, physical code)` pairs in top-to-bottom, left-to-right order.
    ///
    /// Only the 30 letter-block keys are included. Digits, modifiers, function keys,
    /// Backspace and CapsLock remain explicit physical [`KeyCode`] bindings. This
    /// avoids ambiguous labels from OS remaps or shifted/AltGr characters.
    pub fn keys(self) -> impl ExactSizeIterator<Item = (&'static str, &'static str)> {
        let labels = match self {
            Self::Qwerty => [
                "Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P", "A", "S", "D", "F", "G", "H", "J", "K",
                "L", ";", "Z", "X", "C", "V", "B", "N", "M", ",", ".", "/",
            ],
            Self::Colemak => [
                "Q", "W", "F", "P", "G", "J", "L", "U", "Y", ";", "A", "R", "S", "T", "D", "H", "N", "E",
                "I", "O", "Z", "X", "C", "V", "B", "K", "M", ",", ".", "/",
            ],
            Self::Azerty => [
                "A", "Z", "E", "R", "T", "Y", "U", "I", "O", "P", "Q", "S", "D", "F", "G", "H", "J", "K",
                "L", "M", "W", "X", "C", "V", "B", "N", ",", ";", ":", "!",
            ],
        };
        labels.into_iter().zip(PHYSICAL)
    }

    /// Resolves one letter-block label to a physical key, ignoring ASCII case.
    ///
    /// Unknown labels, physical code strings, chords and shifted characters return
    /// `None`; use [`KeyCode::new`] directly for an already physical binding.
    pub fn physical_key(self, label: &str) -> Option<KeyCode> {
        self.keys()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(label))
            .map(|(_, physical)| KeyCode::new(physical))
    }

    /// The preset's uppercase/unshifted label for a physical letter-block key.
    ///
    /// Returns `None` outside the letter block. The host can fall back to
    /// [`KeyCode::as_str`] for those keys.
    pub fn key_label(self, key: &KeyCode) -> Option<&'static str> {
        self.keys()
            .find(|(_, physical)| *physical == key.as_str())
            .map(|(label, _)| label)
    }

    /// Positional movement in up/down/left/right order, ready for a host's action.
    ///
    /// Every layout uses the same physical W/S/A/D positions. Their labels are
    /// W/S/A/D on QWERTY, W/R/A/S on Colemak and Z/S/Q/D on AZERTY. This builds a
    /// source only; the host chooses the action and whether to replace any binding.
    pub fn movement_keys(self) -> InputSource {
        InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")
    }
}
