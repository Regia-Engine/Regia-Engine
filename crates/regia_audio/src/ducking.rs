// SPDX-License-Identifier: MPL-2.0
//! Ducking: bus volume rules driven by what is playing (`docs/specs/16-audio.md` § 1).
//!
//! A rule says "when activity on `when` is present, reduce `target` by `amount_db`, ramping with
//! `attack`/`release`". The engine keeps one envelope per rule (0..1, linear in dB) and sums the
//! resulting offsets per target bus. Activity sources are bus names plus the synthetic `"Voice"`
//! (voice chat) and `"Announcer"` keys maintained by the voice manager and announcer.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One ducking rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DuckRule {
    /// Activity source that triggers the rule: a bus name, `"Voice"` (voice chat) or
    /// `"Announcer"`.
    pub when: String,
    /// Bus whose volume is reduced.
    pub target: String,
    /// Reduction in dB (negative).
    pub amount_db: f32,
    /// Ramp time from 0 to full reduction.
    pub attack: Duration,
    /// Ramp time from full reduction back to 0.
    pub release: Duration,
}

/// The spec's example rule plus the DO27 announcer rules (game −6 dB under announcer, 300 ms
/// release; voice chat exempt from the announcer duck).
pub fn default_duck_rules() -> Vec<DuckRule> {
    vec![
        // Spec 16 § 1: "when Voice active, Music -6 dB, attack 0.2, release 1.0".
        DuckRule {
            when: "Voice".into(),
            target: "Music".into(),
            amount_db: -6.0,
            attack: Duration::from_secs_f64(0.2),
            release: Duration::from_secs_f64(1.0),
        },
        // DO27: announcer ducks the game buses, short attack, 300 ms release.
        DuckRule {
            when: "Announcer".into(),
            target: "Music".into(),
            amount_db: -6.0,
            attack: Duration::from_secs_f64(0.05),
            release: Duration::from_secs_f64(0.3),
        },
        DuckRule {
            when: "Announcer".into(),
            target: "Sfx".into(),
            amount_db: -6.0,
            attack: Duration::from_secs_f64(0.05),
            release: Duration::from_secs_f64(0.3),
        },
        DuckRule {
            when: "Announcer".into(),
            target: "Ambience".into(),
            amount_db: -6.0,
            attack: Duration::from_secs_f64(0.05),
            release: Duration::from_secs_f64(0.3),
        },
    ]
}

/// Tracks the envelopes of a fixed rule list.
#[derive(Debug, Default)]
pub struct DuckEngine {
    envelopes: Vec<f32>,
    /// Summed offset in dB per target bus. Empty while nothing is ducked.
    offsets: HashMap<String, f32>,
}

impl DuckEngine {
    /// An engine for `rules` (the list is fixed at construction; changing rules rebuilds).
    pub fn new(rules: &[DuckRule]) -> Self {
        Self {
            envelopes: vec![0.0; rules.len()],
            offsets: HashMap::new(),
        }
    }

    /// Advances every envelope by `dt` seconds given the current activity counts per source
    /// (`"Music"`, `"Announcer"`, ...). Returns the summed offset in dB per target bus.
    ///
    /// The returned map is a clone of the offsets `advance` keeps. A steady mix that
    /// is not ducking clones an empty map.
    pub fn tick(
        &mut self,
        dt: f64,
        rules: &[DuckRule],
        activity: &HashMap<String, u32>,
    ) -> HashMap<String, f32> {
        self.advance(dt, rules, activity);
        self.offsets.clone()
    }

    /// [`Self::tick`] without cloning the offset map.
    pub(crate) fn advance(&mut self, dt: f64, rules: &[DuckRule], activity: &HashMap<String, u32>) {
        for offset in self.offsets.values_mut() {
            *offset = 0.0;
        }
        // A NaN or negative step would run an envelope backwards or, as NaN, stick it for good
        // (`clamp` passes NaN through); it holds the envelopes instead.
        let dt = if dt.is_nan() || dt < 0.0 { 0.0 } else { dt };
        for (rule, envelope) in rules.iter().zip(&mut self.envelopes) {
            let active = activity.get(&rule.when).is_some_and(|&count| count > 0);
            let delta = if active {
                if rule.attack.is_zero() {
                    1.0
                } else {
                    (dt / rule.attack.as_secs_f64()) as f32
                }
            } else if rule.release.is_zero() {
                -1.0
            } else {
                -(dt / rule.release.as_secs_f64()) as f32
            };
            *envelope = (*envelope + delta).clamp(0.0, 1.0);
            if *envelope > 0.0 {
                let add = rule.amount_db * *envelope;
                if let Some(offset) = self.offsets.get_mut(rule.target.as_str()) {
                    *offset += add;
                } else {
                    self.offsets.insert(rule.target.clone(), add);
                }
            }
        }
        self.offsets.retain(|_, offset| *offset != 0.0);
    }

    /// Offsets from the last [`Self::advance`].
    pub(crate) fn offsets(&self) -> &HashMap<String, f32> {
        &self.offsets
    }

    /// The current envelope of rule `index` (1 = fully ducked).
    pub fn envelope(&self, index: usize) -> f32 {
        self.envelopes.get(index).copied().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice_music_rule() -> Vec<DuckRule> {
        vec![DuckRule {
            when: "Voice".into(),
            target: "Music".into(),
            amount_db: -6.0,
            attack: Duration::from_secs_f64(0.2),
            release: Duration::from_secs_f64(1.0),
        }]
    }

    #[test]
    fn spec_example_rule_ramps_with_attack_and_release() {
        let rules = voice_music_rule();
        let mut engine = DuckEngine::new(&rules);
        let mut activity = HashMap::new();
        activity.insert("Voice".to_string(), 1);

        // Attack: linear in dB, -3 dB at half the attack, -6 dB at and after the full attack.
        let mut offsets = engine.tick(0.1, &rules, &activity);
        assert!(
            (offsets["Music"] + 3.0).abs() < 1e-6,
            "got {:?}",
            offsets["Music"]
        );
        offsets = engine.tick(0.1, &rules, &activity);
        assert!((offsets["Music"] + 6.0).abs() < 1e-6);
        offsets = engine.tick(5.0, &rules, &activity);
        assert!((offsets["Music"] + 6.0).abs() < 1e-6, "clamped at full duck");

        // Release: 1.0 s linear ramp back to 0.
        activity.clear();
        offsets = engine.tick(0.5, &rules, &activity);
        assert!(
            (offsets["Music"] + 3.0).abs() < 1e-6,
            "got {:?}",
            offsets["Music"]
        );
        offsets = engine.tick(0.5, &rules, &activity);
        let released = offsets.get("Music").copied().unwrap_or(0.0);
        assert!(released.abs() < 1e-6, "fully released: {released:?}");
        offsets = engine.tick(0.5, &rules, &activity);
        assert!(!offsets.contains_key("Music"), "stays at zero");
    }

    #[test]
    fn zero_activity_ducks_nothing() {
        let rules = default_duck_rules();
        let mut engine = DuckEngine::new(&rules);
        let offsets = engine.tick(1.0, &rules, &HashMap::new());
        assert!(offsets.is_empty());
    }

    #[test]
    fn announcer_rules_duck_the_game_buses_not_voice() {
        let rules = default_duck_rules();
        let mut engine = DuckEngine::new(&rules);
        let mut activity = HashMap::new();
        activity.insert("Announcer".to_string(), 1);
        for _ in 0..100 {
            engine.tick(0.01, &rules, &activity);
        }
        let offsets = engine.tick(0.01, &rules, &activity);
        for bus in ["Music", "Sfx", "Ambience"] {
            assert!((offsets[bus] + 6.0).abs() < 1e-4, "{bus} should be fully ducked");
        }
        assert!(!offsets.contains_key("Voice"), "voice chat is exempt");
    }

    #[test]
    fn multiple_rules_on_one_bus_sum() {
        let rules = vec![
            DuckRule {
                when: "Voice".into(),
                target: "Music".into(),
                amount_db: -6.0,
                attack: Duration::ZERO,
                release: Duration::from_secs_f64(1.0),
            },
            DuckRule {
                when: "Announcer".into(),
                target: "Music".into(),
                amount_db: -4.0,
                attack: Duration::ZERO,
                release: Duration::from_secs_f64(1.0),
            },
        ];
        let mut engine = DuckEngine::new(&rules);
        let mut activity = HashMap::new();
        activity.insert("Voice".to_string(), 2);
        activity.insert("Announcer".to_string(), 1);
        let offsets = engine.tick(0.016, &rules, &activity);
        assert!((offsets["Music"] + 10.0).abs() < 1e-6);
    }
}
