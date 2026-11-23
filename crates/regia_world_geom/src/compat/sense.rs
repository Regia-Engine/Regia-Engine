// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/tunnel_sense.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-106, mark: as-is (Stage A: unchanged; the file is the same at regia-absorb-1).
// Edits: module paths in doc links, doc comments (DO27 plan names dropped, missing docs added).

//! Who hears, sees and takes what underground (the tunnel sense rules).
//!
//! * Sound carries further underground: engines and gunfire reach a listener
//!   inside at a longer range, with direction (the caller keeps the bearing;
//!   only the range changes here).
//! * No satellite view underground: satellite mode and the map show only the
//!   tunnel entrances, never what is inside. Units inside are reported by
//!   voice only (the voice path is untouched: this filter only hides them
//!   from the overhead views).
//! * Salvage only underground: caches that can be reached only through the
//!   tunnels are picked up only by a unit that is actually down there.
//! * Kept from today: rock blocks radar (the 150 m rule lives in
//!   [`crate::compat::state::TunnelsState::rock_hidden`]); no geysers, pads or
//!   buildings inside.
//!
//! All pure functions; underground-ness comes from
//! [`crate::compat::state::TunnelsState::underground`].

/// Hearing range multiplier for a listener inside (engines and gunfire carry
/// further down there). Direction is preserved: range only.
pub const UNDERGROUND_HEAR_MULT: u32 = 2;

/// Hearing range for a sound of `base_range_m`: doubled for a listener
/// underground, unchanged otherwise. Integer math (multiply then divide is
/// exact here: the multiplier is integral).
pub fn hear_range_m(base_range_m: u32, listener_underground: bool) -> u32 {
    if listener_underground {
        base_range_m.saturating_mul(UNDERGROUND_HEAR_MULT)
    } else {
        base_range_m
    }
}

/// Satellite mode and the map show a blip when it is not hidden inside the
/// rock: surface units always show, tunnel-entrance markers always show
/// (they are how you find the tunnels), anything else underground never
/// does. Units inside are voice-only. Team does not matter: even allies
/// inside report by voice.
pub fn satellite_shows(target_underground: bool, is_entrance: bool) -> bool {
    is_entrance || !target_underground
}

/// An underground-only cache (scrap or the salvage wreck) is picked up only
/// by a unit that is underground. Surface caches are unaffected.
pub fn can_salvage(site_underground_only: bool, picker_underground: bool) -> bool {
    !site_underground_only || picker_underground
}

/// Structures that can never be placed inside: geysers, pads, buildings.
/// (Kept from today: nothing is built in the tunnels.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsideStructure {
    /// A resource geyser.
    Geyser,
    /// A landing or launch pad.
    Pad,
    /// Any building.
    Building,
}

/// Always false: no geysers, pads or buildings inside.
pub fn may_build_inside(_kind: InsideStructure) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_carries_further_for_a_listener_inside() {
        assert_eq!(hear_range_m(300, false), 300);
        assert_eq!(hear_range_m(300, true), 600);
        assert_eq!(hear_range_m(0, true), 0);
        assert_eq!(hear_range_m(u32::MAX, true), u32::MAX);
    }

    #[test]
    fn satellite_shows_entrances_only_never_what_is_inside() {
        // Surface war, surface wreck: shown.
        assert!(satellite_shows(false, false));
        // A mouth set piece on the surface: shown (it is how you find them).
        assert!(satellite_shows(false, true));
        // An underground unit, even one sitting in an entrance shaft: hidden.
        assert!(!satellite_shows(true, false));
        // The entrance marker itself always shows: it is the way in, not
        // what is inside.
        assert!(satellite_shows(true, true));
    }

    #[test]
    fn underground_caches_need_an_underground_picker() {
        assert!(can_salvage(false, false));
        assert!(can_salvage(false, true));
        assert!(!can_salvage(true, false), "surface picker cannot reach it");
        assert!(can_salvage(true, true));
    }

    #[test]
    fn nothing_is_built_inside() {
        for kind in [
            InsideStructure::Geyser,
            InsideStructure::Pad,
            InsideStructure::Building,
        ] {
            assert!(!may_build_inside(kind), "{kind:?} stays outside");
        }
    }
}
