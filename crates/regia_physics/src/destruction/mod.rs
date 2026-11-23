// SPDX-License-Identifier: MPL-2.0
//! Destruction level 2: pre-fractured chunks held together by breakable bonds (ADR-1597).
//!
//! - [`asset`]: the `.destructible.ron` v2 chunk asset ([`DestructibleAsset`]).
//! - [`sim`]: the deterministic break model ([`DestructionWorld`]): hits ([`Damage`]) load bonds,
//!   bonds break past their threshold (capped per tick), and chunks that lose every path to a
//!   support chunk collapse. Breaks come out as [`BreakEvent`]s (ids, ticks, seeds) a server sends
//!   and a [`Authority::Replica`] replays, plus [`DestructionFx`] sound/VFX hooks.
//! - [`load`]: structural load ([`LoadSettings`]): gravity relaxed through the bonds to the
//!   supports, overloaded bonds fail (ADR-2734).
//! - [`bridge`]: the rapier side ([`DestructionPhysics`]): intact hull colliders, pooled and capped
//!   debris bodies, despawn rules, collider and navmesh invalidation.
//! - [`plugin`]: the ECS resource ([`Destruction`]), [`DestructionPlugin`] and the messages.
//! - [`sample`]: the wall-and-bridge sample scene and its hit script.
//! - [`impact`]: impact damage ([`DestructionImpact`]): contacts on opted-in structures become
//!   hits (ADR-3272).
//!
//! - [`source`]: destructibles placed in a level ([`DestructibleSource`], ADR-9750): the asset is
//!   read and defined at the entity's pose.
//!
//! Level 3 (runtime fracture, carving) is not here. Guide: `book/agents/destruction.md`. Format:
//! `docs/formats/destructible.md`.

pub mod asset;
pub mod bridge;
pub mod impact;
pub mod load;
pub mod plugin;
pub mod sample;
pub mod sim;
pub mod source;

pub use asset::{
    BoxGrid, DESTRUCTIBLE_FORMAT_VERSION, DESTRUCTIBLE_TYPE_PATH, DestructibleAsset, DestructibleBond,
    DestructibleChunk, DestructibleError, DestructionMaterial, FallingSettings, GridSupport, LoadSettings,
    MAX_DESTRUCTIBLE_BONDS, MAX_DESTRUCTIBLE_CHUNKS, MAX_DESTRUCTIBLE_STRING, MAX_HULL_POINTS,
    MAX_HULLS_PER_CHUNK, MAX_LOAD_ITERATIONS, SourceKit, destructible_migration_sources,
    destructible_schema_fingerprint, migrate_destructible,
};
pub use bridge::{
    ActiveChunk, ActiveSection, BridgeReport, BridgeStats, DestructionPhysics, NavInvalidation, SectionSplit,
    add_fixed_box, step_standalone,
};
pub use impact::{
    DestructionImpact, ImpactError, ImpactHit, ImpactReport, ImpactSettings, ImpactTotals,
    MAX_IMPACT_COOLDOWN_TICKS, MAX_IMPACT_COOLDOWNS, MAX_IMPACT_HITS_PER_TICK, convert_impacts,
};
pub use load::{BondLoad, LoadReport, LoadSolution, LoadStats, MIN_LOAD_AREA, solve_load};
pub use plugin::{
    DestructibleRoot, Destruction, DestructionBroke, DestructionChunk, DestructionFxMessage,
    DestructionPlugin, MessageTotals, NavmeshInvalidated, TickSummary, follow_root_transforms,
    step_destruction, write_back_destruction, write_back_free_roots,
};
pub use sim::{
    Authority, BreakEvent, BreakKind, ChunkState, CompactBreak, Damage, DamageKind, DestructibleId,
    DestructibleInstance, DestructiblePose, DestructionBudget, DestructionCounters, DestructionError,
    DestructionFx, DestructionSnapshot, DestructionWorld, DetachCause, EVENT_LOG_CAP, FxKind,
    InstanceSnapshot, MAX_EVENTS_PER_CALL, MAX_QUEUED_DAMAGE, SpawnRequest, StepReport, fold_event_digest,
};
pub use source::{
    DestructibleSource, DestructibleSourceStatus, define_destructible_sources, load_destructible_source,
};
