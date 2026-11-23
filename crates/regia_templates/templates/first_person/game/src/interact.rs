// SPDX-License-Identifier: MIT-0
//! Interaction: look at a door or a button, read the prompt, press E.
//!
//! - [`Interactable`] marks something you can use and says what its prompt reads.
//! - [`find_focus`] picks the interactable you are looking at (close enough and inside a cone
//!   around the view direction) and stores it in [`Focus`]. The HUD turns that into the prompt.
//! - [`use_focus`] flips the focused thing in [`WorldState`] when `Use` is pressed.
//! - [`WorldState`] is the **saved** truth: which doors are open and which buttons are on.
//!   [`sync_from_state`] makes the live [`Door`] and [`Button`] components match it, so a load
//!   puts every door back where it was. [`animate`] slides doors and the lamps they feed.
//!
//! To add an interactable: spawn an entity with `Interactable { id, prompt_on, prompt_off }` and a
//! `Door` or `Button` (or your own component), and give it a branch in [`use_focus`].

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::CameraRig;
use regia_framework::save_game::SaveableResource;
use regia_input::prelude::*;
use regia_scene::Transform;

/// How close the player must be to use something (meters, horizontal).
pub const USE_DISTANCE: f32 = 2.6;
/// Cosine of the half-angle of the cone around the view direction that counts as looking at it.
pub const LOOK_COSINE: f32 = 0.8;
/// How far a door slides up when open (meters).
pub const DOOR_LIFT: f32 = 2.3;
/// How far a lamp rises when its button is on (meters).
pub const LAMP_LIFT: f32 = 0.8;

/// Something the player can use.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct Interactable {
    /// Identifies it across saves (doors and buttons have separate id spaces).
    pub id: u32,
    /// The prompt while it is off / closed.
    pub prompt_off: String,
    /// The prompt while it is on / open.
    pub prompt_on: String,
}

/// A door that slides up when open. `open` mirrors [`WorldState`].
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct Door {
    /// Identifies the door (an `Interactable` has the same id).
    pub id: u32,
    /// Whether it is open.
    pub open: bool,
}

/// A button that switches lamps with the same id. `on` mirrors [`WorldState`].
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct Button {
    /// Identifies the button.
    pub id: u32,
    /// Whether it is on.
    pub on: bool,
}

/// A lamp that rises while the button with its id is on.
#[derive(Component, Debug, Clone, Copy)]
pub struct Lamp {
    /// The button it follows.
    pub button: u32,
    /// Height when off.
    pub base_y: f32,
}

/// The height a door rests at when closed.
#[derive(Component, Debug, Clone, Copy)]
pub struct DoorBase(pub f32);

/// Which doors are open and which buttons are on. Saved with the game.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct WorldState {
    /// Ids of the open doors.
    pub doors_open: Vec<u32>,
    /// Ids of the buttons that are on.
    pub buttons_on: Vec<u32>,
    /// How many times the player used something.
    pub uses: u32,
}

impl SaveableResource for WorldState {}

/// The interactable the player is looking at, if any.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Focus(pub Option<Entity>);

/// `Update`: finds the interactable in front of the player. Horizontal distance and a cone around
/// the view yaw; the closest one wins.
#[tracing::instrument(skip_all, name = "find_focus")]
pub fn find_focus(
    rigs: Query<&CameraRig>,
    players: Query<&Transform, (With<crate::Player>, Without<Interactable>)>,
    things: Query<(Entity, &Transform), With<Interactable>>,
    mut focus: ResMut<Focus>,
) {
    let (Some(rig), Some(player)) = (rigs.iter().next(), players.iter().next()) else {
        return;
    };
    let forward = Vec3::new(-rig.yaw.sin(), 0.0, -rig.yaw.cos());
    let mut best: Option<(Entity, f32)> = None;
    for (entity, transform) in &things {
        let to = (transform.translation - player.translation).with_y(0.0);
        let distance = to.length();
        if distance > USE_DISTANCE || (distance > 0.05 && to.normalize_or_zero().dot(forward) < LOOK_COSINE) {
            continue;
        }
        if best.is_none_or(|(_, nearest)| distance < nearest) {
            best = Some((entity, distance));
        }
    }
    let found = best.map(|(entity, _)| entity);
    if focus.0 != found {
        focus.0 = found;
    }
}

/// `Update`: `Use` flips the focused door or button in [`WorldState`].
pub fn use_focus(
    actions: Res<Actions>,
    focus: Res<Focus>,
    doors: Query<&Door>,
    buttons: Query<&Button>,
    mut state: ResMut<WorldState>,
) {
    if !actions.just_pressed_this_frame("Use") {
        return;
    }
    let Some(entity) = focus.0 else {
        return;
    };
    let flip = |list: &mut Vec<u32>, id: u32| {
        if let Some(at) = list.iter().position(|v| *v == id) {
            list.remove(at);
        } else {
            list.push(id);
        }
    };
    if let Ok(door) = doors.get(entity) {
        flip(&mut state.doors_open, door.id);
        state.uses += 1;
    } else if let Ok(button) = buttons.get(entity) {
        flip(&mut state.buttons_on, button.id);
        state.uses += 1;
    }
}

/// `Update`: makes every [`Door`] and [`Button`] match [`WorldState`] (after a use or a load).
pub fn sync_from_state(state: Res<WorldState>, mut doors: Query<&mut Door>, mut buttons: Query<&mut Button>) {
    for mut door in &mut doors {
        let open = state.doors_open.contains(&door.id);
        if door.open != open {
            door.open = open;
        }
    }
    for mut button in &mut buttons {
        let on = state.buttons_on.contains(&button.id);
        if button.on != on {
            button.on = on;
        }
    }
}

/// `Update`: slides doors up or down and raises or lowers the lamps.
pub fn animate(
    time: Res<Time>,
    buttons: Query<&Button>,
    mut doors: Query<(&Door, &DoorBase, &mut Transform), Without<Lamp>>,
    mut lamps: Query<(&Lamp, &mut Transform), Without<Door>>,
) {
    let step = 4.0 * time.delta_secs();
    for (door, base, mut transform) in &mut doors {
        let target = base.0 + if door.open { DOOR_LIFT } else { 0.0 };
        transform.translation.y += (target - transform.translation.y).clamp(-step, step);
    }
    for (lamp, mut transform) in &mut lamps {
        let on = buttons.iter().any(|button| button.id == lamp.button && button.on);
        let target = lamp.base_y + if on { LAMP_LIFT } else { 0.0 };
        transform.translation.y += (target - transform.translation.y).clamp(-step, step);
    }
}

/// The prompt text for the focused interactable (`Press E to open the gate`), or empty.
#[must_use]
pub fn prompt_for(interactable: &Interactable, on: bool) -> String {
    let text = if on {
        &interactable.prompt_on
    } else {
        &interactable.prompt_off
    };
    format!("Press E to {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_follows_the_state() {
        let gate = Interactable {
            id: 0,
            prompt_off: "open the gate".into(),
            prompt_on: "close the gate".into(),
        };
        assert_eq!(prompt_for(&gate, false), "Press E to open the gate");
        assert_eq!(prompt_for(&gate, true), "Press E to close the gate");
    }
}
