// SPDX-License-Identifier: MPL-2.0
//! The sound of a glass breach (`regia_vfx::glass`): gives every breach sound emitter
//! ([`BreachSoundEmitter`]) a ground-coupled audio emitter. `regia_vfx` and `regia_audio` are
//! the same layer and may not depend on each other, so the join lives here.
//!
//! A breach is a structural failure, coupled into the building and the ground
//! (`EmitterCategory::Collapse`): on an airless world the medium removes the airborne path and the
//! breach is felt, not heard; on Mars and Earth both paths play. Games start their clip on the
//! emitter that `GlassBreachFired::sound_emitter` names.

use regia_app::prelude::*;
use regia_audio::{AudioEmitter, AudioPropagation, EmitterCategory};
use regia_vfx::glass::BreachSoundEmitter;

/// The audio propagation of a breach (`EmitterCategory::Collapse`: airborne + ground-coupled).
#[must_use]
pub fn breach_propagation() -> AudioPropagation {
    AudioPropagation::for_category(EmitterCategory::Collapse)
}

/// Adds the audio emitter and the ground-coupled propagation to new breach sound emitters.
pub fn attach_breach_audio(
    mut commands: Commands,
    added: Query<(Entity, &BreachSoundEmitter), Added<BreachSoundEmitter>>,
) {
    for (entity, breach) in &added {
        commands.entity(entity).insert((
            AudioEmitter {
                min_distance: breach.min_distance,
                max_distance: breach.max_distance,
                ..AudioEmitter::default()
            },
            breach_propagation(),
        ));
    }
}

/// Installs [`attach_breach_audio`] (add with `regia_vfx::VfxPlugin`).
pub struct GlassBreachAudioPlugin;

impl Plugin for GlassBreachAudioPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, attach_breach_audio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{
        AudioMedium, EmitterFrame, ListenerContact, ListenerCoupling, ListenerFrame, PropagationContext,
        propagate,
    };
    use regia_core::math::Vec3;
    use regia_render::glass::{GlassLevelChanged, GlassType};
    use regia_scene::level::{AtmospherePreset, LevelSettings};
    use regia_vfx::glass::GlassBreachFired;

    #[derive(Resource, Default)]
    struct Collected(Vec<GlassBreachFired>);

    fn collect(mut reader: MessageReader<GlassBreachFired>, mut out: ResMut<Collected>) {
        out.0.extend(reader.read().cloned());
    }

    #[test]
    fn an_airless_pressure_pane_failure_is_silent_in_air_but_felt_through_the_ground() {
        let mut app = App::new();
        app.add_plugins((regia_vfx::VfxPlugin, GlassBreachAudioPlugin));
        let mut level = LevelSettings::default();
        level.atmosphere.preset = AtmospherePreset::Airless;
        app.insert_resource(level)
            .init_resource::<Collected>()
            .add_systems(Update, collect.after(regia_vfx::glass::apply_glass_breach));
        app.update();
        let pane = app.world_mut().spawn_empty().id();
        app.world_mut().write_message(GlassLevelChanged {
            entity: pane,
            from: 2,
            to: 3,
            glass_type: GlassType::PressurePane,
            point: Vec3::new(0.0, 3.0, 0.0),
            normal: Vec3::Z,
            set: "pressure_pane".into(),
        });
        app.update();
        app.update();
        let fired = app.world().resource::<Collected>().0.clone();
        assert_eq!(fired.len(), 1, "one breach");
        let breach = &fired[0];
        let emitter = breach.sound_emitter.expect("sound emitter");
        let propagation = app
            .world()
            .get::<AudioPropagation>(emitter)
            .expect("propagation")
            .clone();
        let params = app.world().get::<AudioEmitter>(emitter).expect("emitter").clone();
        // A pilot standing 40 m away on the ground.
        let listener = ListenerFrame::looking_at(Vec3::new(40.0, 0.0, 0.0), -Vec3::X);
        let coupling = ListenerCoupling {
            contact: ListenerContact::Boots,
            ..ListenerCoupling::default()
        };
        let medium = AudioMedium::airless();
        let ctx = PropagationContext {
            medium: &medium,
            listener: &listener,
            coupling: &coupling,
            coupling_weight: 1.0,
            last_contact: ListenerContact::Boots,
            interiors: &[],
        };
        let frame = EmitterFrame {
            position: breach.breach.position,
            velocity: Vec3::ZERO,
            forward: Vec3::ZERO,
        };
        let paths = propagate(&ctx, &frame, &params, &propagation, false);
        assert!(
            paths.airborne.is_none(),
            "an airless breach has no airborne sound"
        );
        let ground = paths.ground.expect("ground-coupled path");
        assert!(
            ground.audible() && ground.felt > 0.0,
            "the breach must be felt through the ground: {ground:?}"
        );
    }
}
