// SPDX-License-Identifier: MIT-0
//! Save/load through the framework's save flow (`SaveFlowPlugin`, spec 91; ADR-2596).
//!
//! F5 and F9 (and the pause and title menus) write `SaveRequest`s for the `Chapter1` slot; the
//! flow drains them in `Last` and answers with `SaveOutcome`s. The slot file is the same
//! `SaveGame` container as before (`<data dir>/saves/Chapter1.save`); the saveable state is the
//! collectible progress, the combat progress (checkpoint, defeated enemies), the flow's
//! `SaveInfo` and `SaveBlobs`.
//!
//! **Save shape v2.** A save written by this version stamps [`SAVE_SHAPE_KEY`] = 2 in
//! `SaveBlobs`. A save from before combat (v1: `CollectProgress` only, written by
//! `SaveGame::save` directly) has no stamp; [`migrate_loaded`] treats it as v1, resets
//! `CombatProgress` to the start (checkpoint 0, nobody defeated: v1 had neither) and stamps it, so
//! the next save is v2. The collectibles load unchanged.

use regia::prelude::*;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::save_flow::{SaveBlobs, SaveInfo, SaveOp, SaveOutcome, SaveRequest};

/// The sample's save slot.
pub const SLOT: &str = "Chapter1";

/// The `SaveBlobs` key holding the sample's save shape version.
pub const SAVE_SHAPE_KEY: &str = "third_person.save_shape";

/// The save shape this version writes (v1: coins only; v2: coins + combat progress).
pub const SAVE_SHAPE: u32 = 2;

/// Results of the save flow (reflected for agent checks and the HUD).
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct SaveStatus {
    /// Saves that succeeded.
    pub saves: u32,
    /// Loads that succeeded.
    pub loads: u32,
    /// Saves migrated from shape v1 on load.
    pub migrated: u32,
    /// The last error (empty when the last operation succeeded).
    pub last_error: String,
    /// The shape of the last loaded save.
    pub loaded_shape: u32,
}

/// Asks the save flow to save the sample slot (stamps the shape and the slot info first).
pub fn save_now(world: &mut World) {
    if let Some(mut blobs) = world.get_resource_mut::<SaveBlobs>()
        && let Err(err) = blobs.set_value(SAVE_SHAPE_KEY, &SAVE_SHAPE)
    {
        tracing::error!(target: "third_person", "save shape stamp: {err}");
    }
    if let Some(mut info) = world.get_resource_mut::<SaveInfo>() {
        info.display_name = "Chapter 1".into();
        info.level = "third_person".into();
        info.save_count += 1;
    }
    world.write_message(SaveRequest::Save { slot: SLOT.into() });
}

/// Asks the save flow to load the sample slot; [`after_save_flow`] reconciles the world.
pub fn load_now(world: &mut World) {
    world.write_message(SaveRequest::Load { slot: SLOT.into() });
}

/// `PostUpdate` (before the save flow drains its requests in `Last`): when a load is pending,
/// drop the live shape stamp, so a file without one (v1) reads as v1 after the load instead of
/// keeping the running game's stamp (a load replaces only the resources the file holds).
#[tracing::instrument(skip_all, target = "third_person", name = "save_prepare_loads")]
pub fn prepare_loads(world: &mut World, mut cursor: Local<MessageCursor<SaveRequest>>) {
    let loading = world
        .get_resource::<Messages<SaveRequest>>()
        .map(|m| {
            cursor
                .read(m)
                .any(|r| matches!(r, SaveRequest::Load { .. } | SaveRequest::QuickLoad))
        })
        .unwrap_or(false);
    if loading && let Some(mut blobs) = world.get_resource_mut::<SaveBlobs>() {
        blobs.remove(SAVE_SHAPE_KEY);
    }
}

/// The shape of the loaded save: the stamp, or 1 without one.
pub fn loaded_shape(world: &World) -> u32 {
    world
        .get_resource::<SaveBlobs>()
        .and_then(|b| b.get_value::<u32>(SAVE_SHAPE_KEY))
        .and_then(Result::ok)
        .unwrap_or(1)
}

/// After a load: migrates a v1 save to v2 (combat progress back to the start) and stamps it.
/// Returns the shape the file had.
pub fn migrate_loaded(world: &mut World) -> u32 {
    let shape = loaded_shape(world);
    if shape < 2 {
        world.insert_resource(crate::checkpoint::CombatProgress::default());
        if let Some(mut blobs) = world.get_resource_mut::<SaveBlobs>() {
            let _ = blobs.set_value(SAVE_SHAPE_KEY, &SAVE_SHAPE);
        }
        tracing::info!(target: "third_person", "migrated a v{shape} save to v{SAVE_SHAPE}");
    }
    shape
}

/// `Update`: answers the flow's outcomes: after a load, migrate, then reconcile the coins and
/// the enemies to the loaded progress.
#[tracing::instrument(skip_all, target = "third_person", name = "save_outcomes")]
pub fn after_save_flow(world: &mut World, mut cursor: Local<MessageCursor<SaveOutcome>>) {
    let outcomes: Vec<SaveOutcome> = world
        .get_resource::<Messages<SaveOutcome>>()
        .map(|m| cursor.read(m).cloned().collect())
        .unwrap_or_default();
    for outcome in outcomes {
        if outcome.slot != SLOT {
            continue;
        }
        match (&outcome.op, &outcome.result) {
            (SaveOp::Save, Ok(result)) => {
                tracing::info!(target: "third_person", "saved {} bytes", result.bytes_written);
                if let Some(mut status) = world.get_resource_mut::<SaveStatus>() {
                    status.saves += 1;
                    status.last_error.clear();
                }
            }
            (SaveOp::Load, Ok(_)) => {
                let shape = migrate_loaded(world);
                crate::collectible::apply_progress(world);
                crate::enemy::reconcile(world);
                if let Some(mut status) = world.get_resource_mut::<SaveStatus>() {
                    status.loads += 1;
                    status.loaded_shape = shape;
                    if shape < SAVE_SHAPE {
                        status.migrated += 1;
                    }
                    status.last_error.clear();
                }
            }
            (_, Err(err)) => {
                tracing::error!(target: "third_person", "save flow {:?}: {err}", outcome.op);
                if let Some(mut status) = world.get_resource_mut::<SaveStatus>() {
                    status.last_error.clone_from(err);
                }
            }
            (SaveOp::Delete, Ok(_)) => {}
        }
    }
}
