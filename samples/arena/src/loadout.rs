// SPDX-License-Identifier: MIT-0
//! The arena's data-driven combat layer (ADR-2134): gameplay tags (`assets/game.tags.ron`),
//! ammunition attributes, cost / cooldown / refill effects and the rifle and rocket abilities
//! (`regia_framework::abilities`, spec 53), and the weapon chooser (`regia_framework::chooser`,
//! spec 51) a state-tree bot asks before every shot.
//!
//! The files ship inside the binary (`include_str!`), so a headless test, an Android build and the
//! desktop launcher read the same data without file IO. The agent methods `abilities.*`,
//! `chooser.*` and `tags.*` see them under the same paths (`book/agents/arena.md`).

use regia_app::prelude::*;
use regia_core::math::Rng;
use regia_framework::abilities::{
    AbilityAssets, AbilityPlugin, AbilitySet, ApplyEffect, AttrId, GrantId, GrantOptions, Severity,
    StartContext, activate, apply_effect, check_activation, grant_ability,
};
use regia_framework::chooser::{ChooserContext, ChooserPlugin, ChooserValue, Choosers, ResultEntry};
use regia_framework::tags::{TagContainer, TagRegistry, TagTable};

/// The project tag table (`assets/game.tags.ron`).
pub const TAGS_TEXT: &str = include_str!("../assets/game.tags.ron");
/// The pawn attribute list.
pub const ATTRS: &str = "abilities/pawn.attrs.ron";
/// The rifle ability.
pub const RIFLE: &str = "abilities/rifle.ability.ron";
/// The rocket ability.
pub const ROCKET: &str = "abilities/rocket.ability.ron";
/// The instant effect that restocks the magazine (stations and respawns).
pub const RESUPPLY: &str = "effects/resupply.effect.ron";
/// The weapon chooser.
pub const WEAPON_CHOOSER: &str = "choosers/weapon.chooser.ron";

/// The ability assets in load order: effects first, because an ability's cost and cooldown and an
/// effect's chain must already be loaded when it is validated.
pub const ABILITY_ASSETS: [(&str, &str); 8] = [
    (ATTRS, include_str!("../assets/abilities/pawn.attrs.ron")),
    (
        "effects/rifle_cost.effect.ron",
        include_str!("../assets/effects/rifle_cost.effect.ron"),
    ),
    (
        "effects/rocket_cost.effect.ron",
        include_str!("../assets/effects/rocket_cost.effect.ron"),
    ),
    (
        "effects/rifle_cooldown.effect.ron",
        include_str!("../assets/effects/rifle_cooldown.effect.ron"),
    ),
    (
        "effects/rocket_cooldown.effect.ron",
        include_str!("../assets/effects/rocket_cooldown.effect.ron"),
    ),
    (RESUPPLY, include_str!("../assets/effects/resupply.effect.ron")),
    (RIFLE, include_str!("../assets/abilities/rifle.ability.ron")),
    (ROCKET, include_str!("../assets/abilities/rocket.ability.ron")),
];

/// The weapon chooser's text.
pub const WEAPON_CHOOSER_TEXT: &str = include_str!("../assets/choosers/weapon.chooser.ron");

/// The `ammo` attribute.
pub const AMMO: AttrId = AttrId::of("ammo");

/// A pawn's two weapon grants (reflected, so `scene.get_entity` shows the grant ids an agent
/// passes to `abilities.activate` / `abilities.check`).
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component, Default)]
pub struct Loadout {
    /// Grant id of the rifle.
    pub rifle: u32,
    /// Grant id of the rocket.
    pub rocket: u32,
}

/// What the weapon chooser picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeaponPick {
    /// The hitscan rifle.
    Rifle,
    /// The pooled rocket.
    Rocket,
    /// Hold fire (both on cooldown, out of range or out of ammunition).
    Hold,
}

/// Problems found while loading the embedded data (empty when everything loaded). Reflected so an
/// agent can `resources.list` it after a data edit.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct LoadoutDiagnostics {
    /// One line per error: `path: code: message`.
    pub errors: Vec<String>,
}

/// Installs tags, abilities and choosers and loads the arena's data into them.
pub struct LoadoutPlugin;

impl Plugin for LoadoutPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<AbilityPlugin>()) {
            app.add_plugins(AbilityPlugin::default());
        }
        if !app.world().contains_resource::<Choosers>() {
            app.add_plugins(ChooserPlugin);
        }
        app.register_type::<Loadout>()
            .register_type::<LoadoutDiagnostics>();
        // The ability chain runs between the weapons and the damage step, so the arena's fixed step
        // stays one total order (the determinism tests rely on it, ADR-867).
        app.configure_sets(FixedUpdate, AbilitySet::Input.after(crate::ArenaSimSet::Weapons));
        let errors = load_data(app.world_mut());
        for line in &errors {
            tracing::error!(target: "arena", "arena data: {line}");
        }
        app.insert_resource(LoadoutDiagnostics { errors });
        #[cfg(feature = "agent")]
        {
            regia_agent::abilities::install_ability_methods(app.world_mut());
            regia_agent::choosers::install_chooser_methods(app.world_mut());
        }
    }
}

/// Builds the tag registry from [`TAGS_TEXT`].
///
/// # Errors
///
/// The tag table's first problem, as text.
pub fn tag_registry() -> Result<TagRegistry, String> {
    let table = TagTable::from_ron(TAGS_TEXT).map_err(|e| e.to_string())?;
    TagRegistry::from_table(&table).map_err(|e| e.to_string())
}

/// Loads the tag table, the ability assets and the weapon chooser into `world`; returns one line
/// per error (warnings are dropped).
pub fn load_data(world: &mut World) -> Vec<String> {
    let mut errors = Vec::new();
    let registry = match tag_registry() {
        Ok(registry) => Some(registry),
        Err(err) => {
            errors.push(format!("game.tags.ron: {err}"));
            None
        }
    };
    if let Some(mut assets) = world.get_resource_mut::<AbilityAssets>() {
        for (path, text) in ABILITY_ASSETS {
            for diag in assets.load_text(path, text, registry.as_ref(), None) {
                if diag.severity == Severity::Error {
                    errors.push(format!("{path}: {}: {}", diag.code, diag.message));
                }
            }
        }
    }
    if let Some(mut choosers) = world.get_resource_mut::<Choosers>()
        && let Err(issues) = choosers.load_text(WEAPON_CHOOSER, WEAPON_CHOOSER_TEXT)
    {
        for issue in issues.iter().filter(|i| i.is_error()) {
            errors.push(format!("{WEAPON_CHOOSER}: {}: {}", issue.code, issue.message));
        }
    }
    if let Some(registry) = registry {
        world.insert_resource(registry);
    }
    errors
}

/// Gives `pawn` the ammunition attributes, its team tag and the two weapon grants. Returns the
/// grants, or `None` when the data did not load (the pawn then fights without abilities).
pub fn equip(world: &mut World, pawn: Entity, is_bot: bool) -> Option<Loadout> {
    let attributes = world
        .get_resource::<AbilityAssets>()?
        .attributes(ATTRS)?
        .build()
        .ok()?;
    let team = if is_bot { "Team.Bot" } else { "Team.Player" };
    let tags = TagContainer::from_names([team]).unwrap_or_default();
    world.get_entity_mut(pawn).ok()?.insert((attributes, tags));
    let rifle = grant_ability(world, pawn, RIFLE, GrantOptions::default()).ok()?;
    let rocket = grant_ability(world, pawn, ROCKET, GrantOptions::default()).ok()?;
    let loadout = Loadout {
        rifle: rifle.0,
        rocket: rocket.0,
    };
    world.entity_mut(pawn).insert(loadout);
    Some(loadout)
}

/// Equips every pawn in the world (spawn order).
pub fn equip_all(world: &mut World) {
    let mut pawns: Vec<(Entity, bool)> = {
        let mut query = world.query::<(Entity, &crate::pawn::Pawn)>();
        query.iter(world).map(|(e, p)| (e, p.is_bot)).collect()
    };
    pawns.sort_by_key(|(e, _)| e.index());
    for (pawn, is_bot) in pawns {
        if equip(world, pawn, is_bot).is_none() {
            tracing::warn!(target: "arena", "pawn {pawn:?} has no loadout (arena data did not load)");
        }
    }
}

/// The pawn's ammunition (0 without attributes).
pub fn ammo(world: &World, pawn: Entity) -> f32 {
    world
        .get::<regia_framework::abilities::Attributes>(pawn)
        .and_then(|a| a.get(AMMO))
        .unwrap_or(0.0)
}

/// Restocks `pawn` (the station's or the respawn's refill). `source` is the station, if any.
/// Returns whether the effect applied.
pub fn refill(world: &mut World, pawn: Entity, source: Option<Entity>) -> bool {
    if world
        .get::<regia_framework::abilities::Attributes>(pawn)
        .is_none()
    {
        return false;
    }
    let mut request = ApplyEffect::new(RESUPPLY, pawn);
    request.source = source;
    apply_effect(world, request).is_ok()
}

/// Asks the weapon chooser what `pawn` should fire at a target `distance` metres away. The
/// readiness inputs come from the abilities' own activation check (cooldown tags, cost).
pub fn pick_weapon(world: &World, pawn: Entity, distance: f32) -> WeaponPick {
    let Some(loadout) = world.get::<Loadout>(pawn).copied() else {
        return WeaponPick::Hold;
    };
    let ready = |grant: u32| check_activation(world, pawn, GrantId(grant), &StartContext::default()).is_ok();
    let mut ctx = ChooserContext::default();
    ctx.set("distance", ChooserValue::Float(distance));
    ctx.set("ammo", ChooserValue::Int(ammo(world, pawn).floor() as i64));
    ctx.set("rifle_ready", ChooserValue::Bool(ready(loadout.rifle)));
    ctx.set("rocket_ready", ChooserValue::Bool(ready(loadout.rocket)));
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return WeaponPick::Hold;
    };
    let Some(table) = choosers.get(WEAPON_CHOOSER) else {
        return WeaponPick::Hold;
    };
    // `First` mode draws nothing; the generator is only there to satisfy the signature.
    let mut rng = Rng::new(0);
    match table.evaluate(choosers, &mut ctx, &mut rng, None) {
        Some(outcome) => match outcome.result {
            ResultEntry::Value(ChooserValue::Name(name)) if name == "rifle" => WeaponPick::Rifle,
            ResultEntry::Value(ChooserValue::Name(name)) if name == "rocket" => WeaponPick::Rocket,
            _ => WeaponPick::Hold,
        },
        None => WeaponPick::Hold,
    }
}

/// Starts the picked weapon's ability (pays its cost, starts its cooldown). Returns the weapon to
/// fire, or `None` when the ability refused (the shot is not taken).
pub fn trigger(world: &mut World, pawn: Entity, pick: WeaponPick) -> Option<crate::weapons::Weapon> {
    let loadout = world.get::<Loadout>(pawn).copied()?;
    let (grant, weapon) = match pick {
        WeaponPick::Rifle => (loadout.rifle, crate::weapons::Weapon::Rifle),
        WeaponPick::Rocket => (loadout.rocket, crate::weapons::Weapon::Rocket),
        WeaponPick::Hold => return None,
    };
    activate(world, pawn, GrantId(grant), StartContext::default())
        .ok()
        .map(|()| weapon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, LoadoutPlugin));
        enable_fixed_step(&mut app, 60.0);
        app
    }

    #[test]
    fn every_embedded_asset_loads_without_errors() {
        let app = app();
        let diagnostics = app.world().resource::<LoadoutDiagnostics>();
        assert!(diagnostics.errors.is_empty(), "{:?}", diagnostics.errors);
        let assets = app.world().resource::<AbilityAssets>();
        assert!(assets.ability(RIFLE).is_some());
        assert!(assets.ability(ROCKET).is_some());
        assert!(assets.effect(RESUPPLY).is_some());
        assert!(app.world().resource::<Choosers>().get(WEAPON_CHOOSER).is_some());
        // The registry declares the team tags the pawns carry.
        let registry = app.world().resource::<TagRegistry>();
        let team = regia_framework::tags::Tag::new("Team.Bot").expect("valid name");
        assert!(registry.contains(team), "Team.Bot is declared");
    }

    #[test]
    fn the_chooser_prefers_a_ready_rocket_at_mid_range_and_the_rifle_up_close() {
        let mut app = app();
        let pawn = app.world_mut().spawn_empty().id();
        equip(app.world_mut(), pawn, true).expect("equipped");
        assert_eq!(pick_weapon(app.world(), pawn, 10.0), WeaponPick::Rocket);
        assert_eq!(pick_weapon(app.world(), pawn, 2.0), WeaponPick::Rifle);
        assert_eq!(pick_weapon(app.world(), pawn, 45.0), WeaponPick::Hold);
    }

    #[test]
    fn a_rocket_costs_four_rounds_and_blocks_the_next_shot_until_the_cadence_lapses() {
        let mut app = app();
        let pawn = app.world_mut().spawn_empty().id();
        equip(app.world_mut(), pawn, true).expect("equipped");
        let fired = trigger(app.world_mut(), pawn, WeaponPick::Rocket);
        assert_eq!(fired, Some(crate::weapons::Weapon::Rocket));
        app.update();
        assert_eq!(ammo(app.world(), pawn), 2.0, "6 - 4");
        // The rocket's cooldown chains the 0.6 s shot cadence: nothing is ready right after.
        assert_eq!(pick_weapon(app.world(), pawn, 10.0), WeaponPick::Hold);
        // 0.6 s at 60 Hz is 36 fixed steps; after 40 the rifle is back, the rocket is not.
        let mut steps = 0;
        while pick_weapon(app.world(), pawn, 10.0) == WeaponPick::Hold && steps < 600 {
            app.update();
            steps += 1;
        }
        assert_eq!(pick_weapon(app.world(), pawn, 10.0), WeaponPick::Rifle);
    }

    #[test]
    fn the_magazine_runs_dry_and_a_refill_restocks_it() {
        let mut app = app();
        let pawn = app.world_mut().spawn_empty().id();
        equip(app.world_mut(), pawn, true).expect("equipped");
        let mut shots = 0;
        for _ in 0..2_000 {
            let pick = pick_weapon(app.world(), pawn, 2.0);
            if trigger(app.world_mut(), pawn, pick).is_some() {
                shots += 1;
            }
            app.update();
        }
        assert_eq!(shots, 6, "six rifle rounds, then the chooser holds fire");
        assert_eq!(ammo(app.world(), pawn), 0.0);
        assert_eq!(pick_weapon(app.world(), pawn, 2.0), WeaponPick::Hold);
        assert!(refill(app.world_mut(), pawn, None));
        // `current` is recomputed in the next fixed step.
        app.update();
        assert_eq!(ammo(app.world(), pawn), 6.0, "the refill is clamped to ammo_max");
    }
}
