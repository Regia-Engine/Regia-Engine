// SPDX-License-Identifier: MPL-2.0
//! What made this session not reproducible (ADR-9850 (b), `specs/57` § 7).
//!
//! Some things change behavior in the middle of a session in a way a replay cannot repeat: a hot
//! reload of an extension swaps code between two frames. [`SessionDeterminism`] is the record of
//! them. [`mark_nondeterministic`] appends an event; the replay and determinism tooling read the
//! record and report it: a demo recorded in a marked session carries the codes in
//! `DemoHeader::nondeterministic` (so a later replay knows its keyframe hashes need not match),
//! and `sim.state_hash` answers them beside the hash.
//!
//! The record only grows: nothing in a session can make it reproducible again.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// The code of an extension hot reload (`plugin.reload`, or the editor's reload of a dynamic
/// extension).
pub const NONDETERMINISTIC_RELOAD: &str = "nondeterministic_reload";

/// One event that made the session nondeterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NondeterminismEvent {
    /// Lowercase code ([`NONDETERMINISTIC_RELOAD`]).
    pub code: String,
    /// What caused it (an extension id).
    pub source: String,
    /// Free text (for a reload, the generation).
    pub detail: String,
}

impl NondeterminismEvent {
    /// `code:source`, the short form demo headers carry.
    #[must_use]
    pub fn tag(&self) -> String {
        format!("{}:{}", self.code, self.source)
    }
}

/// Every event that made this session nondeterministic, in the order they happened. Absent or
/// empty: nothing known broke reproducibility.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeterminism {
    /// The events.
    pub events: Vec<NondeterminismEvent>,
}

impl SessionDeterminism {
    /// Whether nothing broke reproducibility.
    #[must_use]
    pub fn is_deterministic(&self) -> bool {
        self.events.is_empty()
    }

    /// The distinct [`NondeterminismEvent::tag`]s, sorted (what a demo header records).
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = self.events.iter().map(NondeterminismEvent::tag).collect();
        tags.sort();
        tags.dedup();
        tags
    }
}

/// Records that the session stopped being reproducible (creates the resource on first use) and
/// logs it once at `warn` under the code.
pub fn mark_nondeterministic(world: &mut World, code: &str, source: &str, detail: impl Into<String>) {
    let detail = detail.into();
    tracing::warn!(target: "regia::session", code, source, detail = %detail, "session marked nondeterministic");
    world
        .get_resource_or_init::<SessionDeterminism>()
        .events
        .push(NondeterminismEvent {
            code: code.to_owned(),
            source: source.to_owned(),
            detail,
        });
}

/// The record's tags, empty when the world has none.
#[must_use]
pub fn session_tags(world: &World) -> Vec<String> {
    world
        .get_resource::<SessionDeterminism>()
        .map(SessionDeterminism::tags)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_accumulate_and_tags_dedup() {
        let mut world = World::new();
        assert!(session_tags(&world).is_empty());
        mark_nondeterministic(&mut world, NONDETERMINISTIC_RELOAD, "b_ext", "generation 1");
        mark_nondeterministic(&mut world, NONDETERMINISTIC_RELOAD, "a_ext", "generation 1");
        mark_nondeterministic(&mut world, NONDETERMINISTIC_RELOAD, "b_ext", "generation 2");
        let record = world.resource::<SessionDeterminism>();
        assert!(!record.is_deterministic());
        assert_eq!(record.events.len(), 3);
        assert_eq!(
            session_tags(&world),
            ["nondeterministic_reload:a_ext", "nondeterministic_reload:b_ext"]
        );
    }
}
