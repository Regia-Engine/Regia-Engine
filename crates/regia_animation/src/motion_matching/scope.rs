// SPDX-License-Identifier: MPL-2.0
//! Montage-scoped matching (`docs/specs/52-motion-matching.md` § 2.5a, ADR-2858): while a montage
//! plays a given section, the matcher searches only the frames a tag filter allows (for example
//! only `"stagger"` frames during the montage's `hit` section), on top of its own
//! [`super::MotionMatcher::filter`].
//!
//! A [`MotionMatchMontageScope`] lists rules (montage name, section name, filter).
//! [`run_montage_scopes`] reads the [`MontagePlayer`] on the same entity every frame and sets
//! [`super::MotionMatcher::scope`] to the first rule whose montage is playing that section
//! (blending out does not count), or clears it. The scope is strict: if it leaves no eligible
//! frame the matcher keeps playing its continuation and reports `no_candidates` (only the very
//! first pick ignores a filter, so a character is never left without a clip).

// Source: original design (tag filters are spec 52 § 2.5; this only switches them by montage
// section). Original code; nothing ported.

use regia_ecs::prelude::*;

use super::{MotionMatching, SearchFilter};
use crate::montage_player::{MontagePhase, MontagePlayer, MontageStatus};

/// One scope rule: while `montage` plays `section`, searches must pass `filter`.
#[derive(Clone, Debug, PartialEq)]
pub struct MontageScopeRule {
    /// Montage name; empty matches any montage.
    pub montage: String,
    /// Section name; empty matches any section.
    pub section: String,
    /// The tag filter (built from the database's tag table, [`super::TagTable::mask`]).
    pub filter: SearchFilter,
}

/// Montage-section scopes of an entity's motion matching. Opt-in: add it next to a
/// [`MotionMatching`] and a [`MontagePlayer`].
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MotionMatchMontageScope {
    /// Rules, first match wins.
    pub rules: Vec<MontageScopeRule>,
    /// The rule active after the latest [`run_montage_scopes`] (index into `rules`).
    pub active: Option<usize>,
}

/// The first rule (index and filter) whose montage plays its section among `playing`
/// (instances blending out are ignored); `None` when no rule applies.
pub fn montage_scope_filter(
    rules: &[MontageScopeRule],
    playing: &[MontageStatus],
) -> Option<(usize, SearchFilter)> {
    rules.iter().enumerate().find_map(|(index, rule)| {
        playing
            .iter()
            .filter(|s| s.phase != MontagePhase::BlendingOut)
            .any(|s| {
                (rule.montage.is_empty() || rule.montage == s.montage)
                    && (rule.section.is_empty() || rule.section == s.section)
            })
            .then_some((index, rule.filter))
    })
}

/// Set every scoped matcher's [`super::MotionMatcher::scope`] from its entity's montage player.
/// The [`super::MotionMatchPlugin`] runs it before the matcher.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn run_montage_scopes(
    mut scoped: Query<(
        &mut MotionMatchMontageScope,
        &mut MotionMatching,
        Option<&MontagePlayer>,
    )>,
) {
    for (mut scope, mut matching, player) in &mut scoped {
        let found = player.and_then(|p| montage_scope_filter(&scope.rules, &p.instances()));
        let active = found.map(|(index, _)| index);
        if scope.active != active {
            scope.active = active;
        }
        let filter = found.map(|(_, filter)| filter);
        if matching.matcher.scope != filter {
            matching.matcher.scope = filter;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::montage_player::MontageId;

    fn status(montage: &str, section: &str, phase: MontagePhase) -> MontageStatus {
        MontageStatus {
            id: MontageId(1),
            montage: montage.into(),
            section: section.into(),
            time: 0.0,
            length: 1.0,
            weight: 1.0,
            rate: 1.0,
            phase,
            interrupted: false,
            reverse: false,
        }
    }

    #[test]
    fn the_first_matching_rule_wins_and_blend_out_does_not_count() {
        let hit = SearchFilter {
            require: 0b10,
            forbid: 0,
        };
        let any = SearchFilter {
            require: 0b01,
            forbid: 0,
        };
        let rules = vec![
            MontageScopeRule {
                montage: "react".into(),
                section: "hit".into(),
                filter: hit,
            },
            MontageScopeRule {
                montage: String::new(),
                section: String::new(),
                filter: any,
            },
        ];
        let playing = [status("react", "hit", MontagePhase::Playing)];
        assert_eq!(montage_scope_filter(&rules, &playing), Some((0, hit)));
        let other = [status("react", "recover", MontagePhase::Playing)];
        assert_eq!(montage_scope_filter(&rules, &other), Some((1, any)));
        let leaving = [status("react", "hit", MontagePhase::BlendingOut)];
        assert_eq!(montage_scope_filter(&rules, &leaving), None);
        assert_eq!(montage_scope_filter(&rules, &[]), None);
    }
}
