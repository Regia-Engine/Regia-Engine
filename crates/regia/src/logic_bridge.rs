// SPDX-License-Identifier: MPL-2.0
//! Logic-graph integrations (`docs/specs/102-logic-graph.md` § 13 P3, ADR-7268): the tasks and
//! routes that connect `regia_ai`'s logic graphs to the sequencer, audio cues, input actions, Lua
//! mods and the net layer. They live in the umbrella crate because `regia_ai` depends on none of
//! those systems (and `regia_framework` cannot depend on `regia_ai`).
//!
//! Add [`LogicBridgePlugin`] after (or with) `regia_ai::AiPlugin`. With the features on, it
//! registers into the shared `BtTaskRegistry`:
//!
//! - `logic::PlaySequence` (`client`, presentation): starts the sequence `sequence` (a
//!   `SequenceLibrary` name, usually the `.seq.ron` project path) on a new player entity, binding
//!   `target` (an entity; default: the graph's entity) under the binding name `bind` (default
//!   `"target"`). Markers that player crosses come back to the graph as `OnSequenceMarker` events.
//! - `logic::PlayCue` (`client`, presentation): plays the sound cue `cue` through `Audio::play_cue`
//!   (a registered cue name, or a `.cue.ron` path whose file stem is the name).
//! - `logic::CallLua` (`lua`, sim): calls global function `function` of Lua mod `mod` with the
//!   action's other `set` entries as a table; `false` fails the action, anything else succeeds, and
//!   a Lua error fails only the action (the mod stays enabled).
//!
//! and routes into `LogicInbox`es: sequence markers ([`LogicSequenceSource`]), bound input actions
//! of possessed entities (`PlayerController`), `regia.emit` events of Lua mods (to the event's
//! target, or to entities marked [`LogicModHost`]) and, per message type, applied net messages
//! (`LogicNetAppExt::add_logic_net_message`, `net`). With `net` it installs a `NetRole`-based
//! `LogicAuthority` (`net_logic_authority`).
//!
//! In every build it also registers the match-rules tasks (`rules::AddScore`, `rules::EndMatch`,
//! `rules::SetTeam`, `rules::SetSpawnEnabled`, `rules::LockObjective`) and routes the match
//! session's events into graphs as `match.<kind>` ([`rules`], audit gap GR-7.6).

pub mod rules;

use regia_ai::logic::{LogicEvent, input_action_event};
use regia_ai::{AiStep, BtTaskRegistry, LogicGraph, LogicInbox};
// Used only by the client and Lua tasks (`param` and the `client` / `lua` modules): a
// server-only build (`regia-server-logic` matrix row) has neither.
#[cfg(any(feature = "client", feature = "lua"))]
use regia_ai::{BbValue, BtContext, BtStatus, BtTask};
use regia_app::prelude::*;

/// Registers the logic-graph integration tasks and routes (module docs).
#[derive(Debug, Default, Clone, Copy)]
pub struct LogicBridgePlugin;

impl Plugin for LogicBridgePlugin {
    fn build(&self, app: &mut App) {
        let registry = app.world_mut().get_resource_or_init::<BtTaskRegistry>();
        register_tasks(registry.into_inner());
        app.add_systems(
            FixedUpdate,
            route_input_actions
                .after(regia_input::InputSet::Evaluation)
                .before(AiStep),
        );
        // The marker route reads `MarkerReached`, which exists without the sequencer plugin too (an
        // app that plays no sequence then never sends one).
        #[cfg(feature = "client")]
        app.add_message::<regia_framework::sequencer::MarkerReached>()
            .add_systems(
                PostUpdate,
                route_sequence_markers.after(regia_framework::sequencer::SequencerSet::Advance),
            );
        #[cfg(feature = "lua")]
        {
            app.init_resource::<LogicLuaUnrouted>();
            app.add_systems(FixedUpdate, route_lua_events.before(AiStep));
        }
        #[cfg(feature = "net")]
        app.insert_resource(regia_ai::LogicAuthority(net_logic_authority));
        // Match events of the frame's fixed steps reach graphs on their next tick (GR-7.6). In
        // `PostUpdate`, like the marker route: no order against `AiStep` or a game's damage sets.
        app.add_message::<regia_framework::rules::MatchEvent>()
            .add_systems(PostUpdate, rules::route_match_events);
    }
}

/// Registers the integration tasks of the enabled features into `registry`, marking the
/// presentation ones (what [`LogicBridgePlugin`] does at build).
pub fn register_tasks(registry: &mut BtTaskRegistry) {
    #[cfg(feature = "client")]
    {
        registry.register_factory(PLAY_SEQUENCE, || Box::new(PlaySequence::default()));
        registry.mark_presentation(PLAY_SEQUENCE);
        registry.register_factory(PLAY_CUE, || Box::new(PlayCue::default()));
        registry.mark_presentation(PLAY_CUE);
    }
    #[cfg(feature = "lua")]
    registry.register_factory(CALL_LUA, || Box::new(CallLua::default()));
    rules::register(registry);
}

/// The task path of the sequence action.
pub const PLAY_SEQUENCE: &str = "logic::PlaySequence";
/// The task path of the sound-cue action.
pub const PLAY_CUE: &str = "logic::PlayCue";
/// The task path of the Lua call action.
pub const CALL_LUA: &str = "logic::CallLua";

#[cfg(any(feature = "client", feature = "lua"))]
fn param<'a>(params: &'a [(String, BbValue)], key: &str) -> Option<&'a BbValue> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

#[cfg(any(feature = "client", feature = "lua"))]
fn param_str<'a>(params: &'a [(String, BbValue)], key: &str) -> Option<&'a str> {
    match param(params, key) {
        Some(BbValue::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

// ---- input actions -----------------------------------------------------------------------------------

/// `FixedUpdate`, after input evaluation and before the AI step: for every `PlayerController`, the
/// possessed entity's logic graph gets `input_action:<action>` for each of its `InputAction` arms
/// whose action was just pressed on this fixed step by that controller's player (spec 102 § 4.1).
/// An entity is "locally controlled" here when a local `PlayerController` possesses it.
pub fn route_input_actions(
    controllers: Query<&regia_framework::player::PlayerController>,
    actions: Option<Res<regia_input::Actions>>,
    mut graphs: Query<(&LogicGraph, Option<&mut LogicInbox>)>,
    mut commands: Commands,
) {
    let Some(actions) = actions else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia::logic", "route_input_actions").entered();
    for controller in &controllers {
        let Ok((graph, inbox)) = graphs.get_mut(controller.possessed) else {
            continue;
        };
        let player = actions.player(controller.player_index);
        let mut fired: Vec<String> = Vec::new();
        for rule in &graph.def().rules {
            if let LogicEvent::InputAction { action } = &rule.event
                && player.states.get(action).is_some_and(|s| s.just_pressed())
            {
                let name = input_action_event(action);
                // Two rules on one action raise it once.
                if !fired.contains(&name) {
                    fired.push(name);
                }
            }
        }
        if fired.is_empty() {
            continue;
        }
        match inbox {
            Some(mut inbox) => inbox.events.extend(fired),
            None => {
                commands
                    .entity(controller.possessed)
                    .insert(LogicInbox { events: fired });
            }
        }
    }
}

// ---- sequencer and audio (client) ----------------------------------------------------------------------

/// On a sequence player entity: the logic graph that started it and the sequence name it was started
/// with. Markers the player crosses reach `graph` (and every bound target with a logic graph) as
/// `sequence_marker:<sequence>#<marker>`. `logic::PlaySequence` adds it; a host that starts a
/// sequence itself can add it too.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LogicSequenceSource {
    /// The graph entity to notify.
    pub graph: Entity,
    /// The sequence name the graph's `OnSequenceMarker` arms name.
    pub sequence: String,
}

#[cfg(feature = "client")]
mod client {
    use super::*;
    use regia_framework::sequencer::{
        Binding, MarkerReached, SequenceBindings, SequenceLibrary, SequencePlayer,
    };

    /// `logic::PlaySequence` (module docs).
    #[derive(Default)]
    pub(super) struct PlaySequence {
        params: Vec<(String, BbValue)>,
    }

    impl BtTask for PlaySequence {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let Some(name) = param_str(&self.params, "sequence") else {
                tracing::warn!(target: "regia::logic", "logic::PlaySequence needs a sequence name");
                return BtStatus::Failure;
            };
            let target = match param(&self.params, "target") {
                None => ctx.entity,
                Some(BbValue::Entity(bits)) => match Entity::try_from_bits(*bits) {
                    Some(e) if ctx.world.get_entity(e).is_ok() => e,
                    _ => return BtStatus::Failure,
                },
                Some(_) => return BtStatus::Failure,
            };
            let bind = param_str(&self.params, "bind").unwrap_or("target").to_string();
            let Some(sequence) = ctx
                .world
                .get_resource::<SequenceLibrary>()
                .and_then(|library| library.get(name))
                .cloned()
            else {
                tracing::warn!(target: "regia::logic", sequence = name, "logic::PlaySequence: no such sequence in the SequenceLibrary");
                return BtStatus::Failure;
            };
            let mut player = SequencePlayer::new(sequence);
            player.play();
            ctx.world.spawn((
                player,
                SequenceBindings::default().with(bind, Binding::Entity(target)),
                LogicSequenceSource {
                    graph: ctx.entity,
                    sequence: name.to_string(),
                },
            ));
            BtStatus::Success
        }

        fn logic_params(&mut self, params: &[(String, BbValue)]) {
            self.params.clear();
            self.params.extend_from_slice(params);
        }
    }

    /// `logic::PlayCue` (module docs).
    #[derive(Default)]
    pub(super) struct PlayCue {
        params: Vec<(String, BbValue)>,
    }

    /// `a/b/creak.cue.ron` → `creak`.
    fn cue_stem(path: &str) -> &str {
        let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
        file.strip_suffix(".cue.ron").unwrap_or(file)
    }

    impl BtTask for PlayCue {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let Some(cue) = param_str(&self.params, "cue") else {
                tracing::warn!(target: "regia::logic", "logic::PlayCue needs a cue");
                return BtStatus::Failure;
            };
            let Some(mut audio) = ctx.world.get_resource_mut::<regia_audio::Audio>() else {
                return BtStatus::Failure;
            };
            let result = match audio.play_cue(cue) {
                Err(regia_audio::AudioError::UnknownEntry(_)) if cue_stem(cue) != cue => {
                    audio.play_cue(cue_stem(cue))
                }
                other => other,
            };
            match result {
                // A play refused by the cue's cooldown or voice limit is still a success.
                Ok(_) => BtStatus::Success,
                Err(error) => {
                    tracing::warn!(target: "regia::logic", cue, %error, "logic::PlayCue failed");
                    BtStatus::Failure
                }
            }
        }

        fn logic_params(&mut self, params: &[(String, BbValue)]) {
            self.params.clear();
            self.params.extend_from_slice(params);
        }
    }

    /// `PostUpdate`, after the sequencer advanced: every `MarkerReached` of a player that carries a
    /// [`LogicSequenceSource`] is pushed into the inbox of its graph and of every bound target that
    /// has a logic graph, as `sequence_marker:<sequence>#<marker>`.
    pub fn route_sequence_markers(
        mut markers: MessageReader<MarkerReached>,
        players: Query<(
            &LogicSequenceSource,
            Option<&SequencePlayer>,
            Option<&SequenceBindings>,
        )>,
        mut inboxes: Query<Option<&mut LogicInbox>, With<LogicGraph>>,
        mut commands: Commands,
    ) {
        for marker in markers.read() {
            let Ok((source, player, bindings)) = players.get(marker.player) else {
                continue;
            };
            let event = regia_ai::logic::sequence_marker_event(&source.sequence, &marker.name);
            // The source graph, then every entity bound to the player (by an entity binding, or a
            // target the player resolved this frame), each once.
            let mut targets = vec![source.graph];
            let bound = bindings
                .into_iter()
                .flat_map(SequenceBindings::iter)
                .filter_map(|(_, binding)| match binding {
                    Binding::Entity(e) => Some(e),
                    Binding::Guid(_) => None,
                })
                .chain(
                    player
                        .into_iter()
                        .flat_map(|p| p.target_entities().values().copied()),
                );
            for e in bound {
                if !targets.contains(&e) {
                    targets.push(e);
                }
            }
            for target in targets {
                match inboxes.get_mut(target) {
                    Ok(Some(mut inbox)) => inbox.events.push(event.clone()),
                    Ok(None) => {
                        commands.entity(target).insert(LogicInbox {
                            events: vec![event.clone()],
                        });
                    }
                    Err(_) => {}
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn a_cue_path_reduces_to_its_name() {
            assert_eq!(super::cue_stem("audio/door/creak.cue.ron"), "creak");
            assert_eq!(super::cue_stem("creak"), "creak");
        }
    }
}

#[cfg(feature = "client")]
pub use client::route_sequence_markers;
#[cfg(feature = "client")]
use client::{PlayCue, PlaySequence};

// ---- Lua ---------------------------------------------------------------------------------------------------

/// Marks an entity as a Lua mod's own: `regia.emit(name, value)` without a target from mod `mod_id`
/// reaches this entity's logic graph (spec 102 § 4.4). Mods have no entity of their own, so a game
/// that wants untargeted emits in a graph names one with this component.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LogicModHost {
    /// The mod whose untargeted events this entity receives.
    pub mod_id: String,
}

/// The most events `LogicLuaUnrouted` (feature `lua`) keeps (the oldest go first).
pub const MAX_LOGIC_LUA_UNROUTED: usize = 1024;

/// `regia.emit` events the Lua route drained but no graph took (no target and no matching
/// [`LogicModHost`], or a target without a logic graph): kept for the game, newest last, at most
/// [`MAX_LOGIC_LUA_UNROUTED`].
#[cfg(feature = "lua")]
#[derive(Resource, Debug, Default)]
pub struct LogicLuaUnrouted {
    /// The events, oldest first.
    pub events: Vec<regia_script_lua::EmittedEvent>,
}

#[cfg(feature = "lua")]
mod lua {
    use super::*;
    use regia_script_lua::{EcsModWorld, LuaMods, ScriptValue};

    /// `logic::CallLua` (module docs).
    #[derive(Default)]
    pub(super) struct CallLua {
        params: Vec<(String, BbValue)>,
    }

    fn script_value(value: &BbValue) -> ScriptValue {
        match value {
            BbValue::Bool(b) => ScriptValue::Bool(*b),
            BbValue::Int(i) => ScriptValue::Number(*i as f64),
            BbValue::Float(f) => ScriptValue::Number(f64::from(*f)),
            BbValue::String(s) => ScriptValue::String(s.clone()),
            BbValue::Vec3(v) => ScriptValue::Vec3(v.to_array()),
            BbValue::Entity(bits) => ScriptValue::Entity(*bits),
        }
    }

    impl BtTask for CallLua {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let (Some(mod_id), Some(function)) = (
                param_str(&self.params, "mod"),
                param_str(&self.params, "function"),
            ) else {
                tracing::warn!(target: "regia::logic", "logic::CallLua needs `mod` and `function`");
                return BtStatus::Failure;
            };
            let table: std::collections::BTreeMap<String, ScriptValue> = self
                .params
                .iter()
                .filter(|(k, _)| k != "mod" && k != "function")
                .map(|(k, v)| (k.clone(), script_value(v)))
                .collect();
            if !ctx.world.contains_resource::<LuaMods>() {
                return BtStatus::Failure;
            }
            let result = ctx.world.resource_scope::<LuaMods, _>(|world, mut mods| {
                mods.call_function(
                    &mut EcsModWorld::new(world),
                    mod_id,
                    function,
                    ScriptValue::Map(table),
                )
            });
            match result {
                Ok(ScriptValue::Bool(false)) => BtStatus::Failure,
                Ok(_) => BtStatus::Success,
                Err(error) => {
                    tracing::warn!(target: "regia::logic", mod_id, function, %error, "logic::CallLua failed");
                    BtStatus::Failure
                }
            }
        }

        fn logic_params(&mut self, params: &[(String, BbValue)]) {
            self.params.clear();
            self.params.extend_from_slice(params);
        }
    }

    /// `FixedUpdate`, before the AI step: drains `LuaMods`' emitted events into logic-graph inboxes:
    /// a targeted event to its target, an untargeted one to every [`LogicModHost`] of its mod.
    /// Events no graph takes go to [`LogicLuaUnrouted`].
    ///
    /// Only the event *name* crosses into the inbox: the inbox stays plain strings (spec 102 § 4.1),
    /// so the emitted `value` is dropped here and no blackboard key is written (contrast the net
    /// arm's `net_message_tick`). This is the intended v1 behaviour (ADR-7268 addendum): the mod's
    /// Lua state stays the source of truth — the game reads a value with `LuaMods::call_function`
    /// or from `LogicLuaUnrouted` (unrouted events keep their value) and writes it where graph
    /// conditions read it, and a graph can call back into the mod with `logic::CallLua`.
    pub fn route_lua_events(world: &mut World, mut hosts: Local<Vec<(Entity, String)>>) {
        let Some(mut mods) = world.get_resource_mut::<LuaMods>() else {
            return;
        };
        if mods.emitted_held() == 0 {
            return;
        }
        let _span = tracing::debug_span!(target: "regia::logic", "route_lua_events").entered();
        let events = mods.drain_emitted();
        hosts.clear();
        let mut query = world.query::<(Entity, &LogicModHost)>();
        hosts.extend(query.iter(world).map(|(e, h)| (e, h.mod_id.clone())));
        hosts.sort();
        let mut unrouted = Vec::new();
        for event in events {
            let mut targets: Vec<Entity> = match event.target {
                Some(bits) => Entity::try_from_bits(bits).into_iter().collect(),
                None => hosts
                    .iter()
                    .filter(|(_, m)| *m == event.mod_id)
                    .map(|(e, _)| *e)
                    .collect(),
            };
            targets.retain(|&e| world.get::<LogicGraph>(e).is_some());
            if targets.is_empty() {
                unrouted.push(event);
                continue;
            }
            for target in targets {
                let Ok(mut entity) = world.get_entity_mut(target) else {
                    continue;
                };
                match entity.get_mut::<LogicInbox>() {
                    Some(mut inbox) => inbox.events.push(event.name.clone()),
                    None => {
                        entity.insert(LogicInbox {
                            events: vec![event.name.clone()],
                        });
                    }
                }
            }
        }
        if !unrouted.is_empty() {
            let mut kept = world.get_resource_or_init::<LogicLuaUnrouted>();
            kept.events.extend(unrouted);
            let excess = kept.events.len().saturating_sub(MAX_LOGIC_LUA_UNROUTED);
            kept.events.drain(..excess);
        }
    }
}

#[cfg(feature = "lua")]
use lua::CallLua;
#[cfg(feature = "lua")]
pub use lua::route_lua_events;

// ---- net ---------------------------------------------------------------------------------------------------

/// The `NetRole`-based logic authority (spec 102 § 8): an entity without a `NetRole` is local
/// (authoritative); with one, `regia_net::has_authority` decides (`Authority` and `Local` run sim
/// actions; `Predicted` and `Proxy` run presentation actions only). [`LogicBridgePlugin`] installs it
/// with the `net` feature.
#[cfg(feature = "net")]
pub fn net_logic_authority(entity: Entity, world: &World) -> bool {
    world
        .get::<regia_net::NetRole>(entity)
        .is_none_or(regia_net::has_authority)
}

/// Routes one net message type into logic graphs.
#[cfg(feature = "net")]
pub trait LogicNetAppExt {
    /// Every `M` applied at `NetSet::Apply` whose target entity has a logic graph pushes
    /// `net_message:<M::message_id()>` into its inbox, in application order (the order of
    /// `NetMessages<M>`, a pure function of the packets received), and writes the sender's tick into
    /// the target's blackboard under `net_message_tick` first (the payload convention of spec 102
    /// § 4.1). Runs in `FixedUpdate` after `NetSet::Apply` and before the AI step.
    fn add_logic_net_message<M: regia_net::NetMessage>(&mut self) -> &mut Self;
}

#[cfg(feature = "net")]
impl LogicNetAppExt for App {
    fn add_logic_net_message<M: regia_net::NetMessage>(&mut self) -> &mut Self {
        self.add_systems(
            FixedUpdate,
            route_net_messages::<M>
                .after(regia_net::NetSet::Apply)
                .before(AiStep),
        )
    }
}

/// The route [`LogicNetAppExt::add_logic_net_message`] installs (exclusive, so a host can also run
/// it in its own schedule, such as the spec 70 harness's gameplay schedule).
#[cfg(feature = "net")]
pub fn route_net_messages<M: regia_net::NetMessage>(world: &mut World) {
    let Some(messages) = world.get_resource::<regia_net::message::NetMessages<M>>() else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia::logic", "route_net_messages", message = M::message_id())
        .entered();
    let delivered: Vec<(Entity, u32)> = messages
        .iter()
        .filter_map(|m| Some((m.target?, m.sent_tick.0)))
        .collect();
    let event = regia_ai::logic::net_message_event(M::message_id());
    for (target, tick) in delivered {
        if world.get::<LogicGraph>(target).is_none() {
            continue;
        }
        let Ok(mut entity) = world.get_entity_mut(target) else {
            continue;
        };
        match entity.get_mut::<regia_ai::Blackboard>() {
            Some(mut bb) => {
                bb.insert("net_message_tick", i64::from(tick));
            }
            None => {
                let mut bb = regia_ai::Blackboard::new();
                bb.insert("net_message_tick", i64::from(tick));
                entity.insert(bb);
            }
        }
        match entity.get_mut::<LogicInbox>() {
            Some(mut inbox) => inbox.events.push(event.clone()),
            None => {
                entity.insert(LogicInbox {
                    events: vec![event.clone()],
                });
            }
        }
    }
}
