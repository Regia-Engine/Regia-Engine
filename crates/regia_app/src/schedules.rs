// SPDX-License-Identifier: MPL-2.0
//! Schedule labels of the frame loop (`docs/02-architecture.md` § 5) plus `Startup` and `Lifecycle`.

use regia_ecs::prelude::*;

/// Runs once, before the first frame.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Startup;

/// Time update, frame counters, message buffer swap.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct First;

/// Platform events → input state, asset completions, hot-reload swaps.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PreUpdate;

/// Fixed-step simulation, run 0..N times per frame at [`crate::FixedTime::hz`].
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FixedUpdate;

/// Gameplay.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Update;

/// Late scripts, interpolation, transform propagation, visibility, audio sync, UI layout.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PostUpdate;

/// Copy render-relevant data into the render world.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extract;

/// Render graph execution, submission, present.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Render;

/// Diagnostics, deferred despawns, frame-end cleanup.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Last;

/// Runs outside the frame, synchronously, on every application lifecycle transition (ADR-012 § 2).
/// Work that must finish before the OS callback returns (dropping surfaces on suspend) goes here.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Lifecycle;

/// Runs once after a level finished loading (triggered by the scene system with
/// [`crate::App::run_schedule`] or `World::run_schedule`, outside the frame schedules).
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OnLevelLoaded;

/// Runs once before a level is unloaded (triggered by the scene system).
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OnLevelUnloading;

/// System sets inside [`PostUpdate`], configured to run in declaration order. Systems in the
/// sets before [`PostUpdateSet::TransformPropagate`] run before it; a system added to `PostUpdate`
/// outside the sets has no such edge, so order a `Transform` writer with
/// `.before(PostUpdateSet::TransformPropagate)`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PostUpdateSet {
    /// Late scripts (`Script::late_update`).
    LateScripts,
    /// Physics interpolation and body → transform sync.
    PhysicsSync,
    /// Animation pose → joint transforms.
    AnimationApply,
    /// Local → global transform propagation.
    TransformPropagate,
    /// Visibility and culling inputs.
    Visibility,
    /// Audio listener/emitter sync.
    AudioSync,
    /// UI layout.
    UiLayout,
}

impl PostUpdateSet {
    /// All sets in execution order.
    pub const ORDER: [PostUpdateSet; 7] = [
        Self::LateScripts,
        Self::PhysicsSync,
        Self::AnimationApply,
        Self::TransformPropagate,
        Self::Visibility,
        Self::AudioSync,
        Self::UiLayout,
    ];
}

/// System sets inside [`FixedUpdate`] that more than one layer-2 crate writes or reads, so the
/// crates order against this label instead of depending on each other (02 § 1; the
/// [`PostUpdateSet`] pattern). A system added to `FixedUpdate` outside these sets has no edge
/// against them; order it explicitly (`.before(FixedUpdateSet::Animation)`, …).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixedUpdateSet {
    /// Animation's writes that the fixed step consumes in the same tick: property tracks
    /// advance and apply to components (a kinematic door's `Transform`) before physics syncs
    /// kinematic poses (ADR-4653).
    Animation,
}

/// Every schedule the app creates up front.
pub(crate) fn all() -> [InternedScheduleLabel; 12] {
    [
        Startup.intern(),
        First.intern(),
        PreUpdate.intern(),
        FixedUpdate.intern(),
        Update.intern(),
        PostUpdate.intern(),
        Extract.intern(),
        Render.intern(),
        Last.intern(),
        Lifecycle.intern(),
        OnLevelLoaded.intern(),
        OnLevelUnloading.intern(),
    ]
}
