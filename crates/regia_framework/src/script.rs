// SPDX-License-Identifier: MPL-2.0
//! Scripts (`docs/specs/14-scene-and-framework.md` § B2).
//!
//! A [`Script`] is a Rust behaviour attached to an entity as a boxed instance inside a
//! [`Scripts`] component: `Scripts(SmallVec<[ScriptSlot; 2]>)`, where each [`ScriptSlot`] holds the
//! type path, the instance, and its `started`/`enabled` flags. Scripts are ordinary data — they
//! derive `Reflect`, `Default` and `Clone` so they can be serialised, shown in the inspector and
//! rebuilt by the hot-reload restore (`specs/20` § 3 step 9).
//!
//! # Execution: take, run, reinsert
//!
//! One exclusive system per schedule ([`Update`], [`FixedUpdate`], and a late variant in
//! [`PostUpdate`] before transform propagation) walks every entity with scripts. For each slot it
//! **takes** the boxed script out of the slot (a pointer move — the archetype does not change),
//! calls the method with a [`ScriptCtx`], and **reinserts** it afterwards. A script whose entity
//! was despawned during its own call (its own `ctx.despawn()`, or anything else) is dropped after
//! [`Script::on_destroy`] instead of being reinserted.
//!
//! # Order
//!
//! Scripts run in **execution order** groups (the `order` of `#[script(order = N)]`, default `0`),
//! then entity spawn order, then slot order within an entity. Deterministic: the same world state
//! always runs scripts in the same order.
//!
//! # Messages and events
//!
//! [`ScriptCtx::send_message`] queues a typed, reflected message. `deliver_messages` runs only
//! at the end of the `Update` pass and takes the queue once, so a message sent from `update`
//! arrives in `on_message` after every `update` call of that pass. A message sent from
//! `on_message`, `on_event`, or `late_update` waits until the next `Update`. [`ScriptEvent`]s
//! (physics collisions, triggers) are delivered the same way from the [`ScriptEvents`] resource:
//! `regia_physics` messages are forwarded by [`ScriptPlugin`]'s bridge system, targeted at the
//! entities the physics event names (`TriggerEnter` → the trigger's scripts only; a collision or
//! a contact force → both participants), while [`broadcast_script_event`] delivers to every script.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_ecs::ecs::component::Mutable;
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use smallvec::SmallVec;

/// The reflection trait the `#[derive(Script)]` accessor names (re-exported so the generated
/// code needs no direct `bevy_reflect` dependency).
#[doc(no_inline)]
pub use regia_ecs::reflect::PartialReflect;

/// Sets that order the script systems relative to other per-frame work.
///
/// - [`TimerPlugin`](crate::timers::TimerPlugin) ticks **before** [`ScriptsSet::FixedUpdate`], so a
///   timer's message is delivered in the same application frame.
/// - Other gameplay systems run relative to these sets to avoid ambiguities with the exclusive
///   script systems.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptsSet {
    /// Script `fixed_update` calls (in [`FixedUpdate`]).
    FixedUpdate,
    /// Script `update` calls (in [`Update`]).
    Update,
    /// Script `late_update` calls (in [`PostUpdate`], set [`PostUpdateSet::LateScripts`]).
    Late,
}

// ---------------------------------------------------------------------------
// The Script trait
// ---------------------------------------------------------------------------

/// A Rust behaviour attached to an entity (§ B2).
///
/// All methods have default no-op bodies; implement what the behaviour needs. Instances are boxed
/// inside [`ScriptSlot`]s, so the trait must be object-safe and `Send + Sync` (components are
/// shared between threads by the scheduler).
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_framework::script::{Script, ScriptCtx};
///
/// #[derive(Default)]
/// struct Door {
///     opened: bool,
/// }
///
/// impl Script for Door {
///     fn update(&mut self, _ctx: &mut ScriptCtx) {
///         self.opened = true;
///     }
/// }
///
/// let mut app = App::new();
/// let entity = app.world_mut().spawn_empty().id();
/// let mut door = Door::default();
/// let mut ctx = ScriptCtx::new(app.world_mut(), entity);
/// door.update(&mut ctx);
/// assert!(door.opened);
/// ```
pub trait Script: std::any::Any + Send + Sync + 'static {
    /// Called once, immediately before the first `update`/`fixed_update`/`late_update` call.
    fn start(&mut self, _ctx: &mut ScriptCtx) {}

    /// Per-frame update ([`Update`] schedule).
    fn update(&mut self, _ctx: &mut ScriptCtx) {}

    /// Fixed-step update ([`FixedUpdate`]).
    fn fixed_update(&mut self, _ctx: &mut ScriptCtx) {}

    /// Late update, after gameplay ([`PostUpdate`], before transform propagation).
    fn late_update(&mut self, _ctx: &mut ScriptCtx) {}

    /// A physics or gameplay event broadcast to this entity's scripts (see [`ScriptEvent`]).
    fn on_event(&mut self, _ctx: &mut ScriptCtx, _event: &ScriptEvent) {}

    /// A typed message from [`ScriptCtx::send_message`] or [`ScriptCtx::broadcast`].
    fn on_message(&mut self, _ctx: &mut ScriptCtx, _message: &dyn Reflect) {}

    /// Called when the script is destroyed: its entity despawned while it ran, or the slot was
    /// removed explicitly.
    fn on_destroy(&mut self, _ctx: &mut ScriptCtx) {}

    /// The script's fields as a reflected value, for UI bindings (`BindSource::ScriptField`,
    /// `specs/18` § 5, ADR-7070). `None` (the default) exposes nothing; a script that derives
    /// `Reflect` returns `Some(self)`.
    fn reflect_fields(&self) -> Option<&dyn regia_ecs::reflect::PartialReflect> {
        None
    }
}

/// Reflection-level description of a script type, generated by `#[derive(Script)]`
/// (`regia_macros`): how to construct a fresh instance and where it sits in the execution order.
///
/// The hot-reload restore constructs scripts through this trait (`specs/20` § 3 step 9).
pub trait ReflectScript: Send + Sync + 'static {
    /// Builds a default instance, boxed as `dyn Script`.
    fn construct() -> Box<dyn Script>
    where
        Self: Sized;

    /// The execution-order group (`#[script(order = N)]`, default `0`).
    fn order() -> i32
    where
        Self: Sized;

    /// The full type path, the key used in [`ScriptRegistry`] and `KindRegistry` presentation
    /// lists (`docs/specs/25-sim-bridge-and-do27.md` § 3).
    fn script_type_path() -> &'static str
    where
        Self: Sized;

    /// The reflected view of `script` when it is an instance of `Self`, else `None`.
    ///
    /// Hot reload reads a script's fields through this (`specs/20` § 3 step 5) and writes them
    /// back into the instance the new library constructs (step 9). The check is by `TypeId`, so
    /// an instance of the same type path from another library build is refused. `#[derive(Script)]`
    /// implements it with [`downcast_reflect`]; the default (a hand-written impl) answers `None`,
    /// and such a script is rebuilt with default fields on reload.
    fn reflect_script(_script: &dyn Script) -> Option<&dyn PartialReflect>
    where
        Self: Sized,
    {
        None
    }

    /// The mutable form of [`ReflectScript::reflect_script`]; `#[derive(Script)]` implements it
    /// with [`downcast_reflect_mut`].
    fn reflect_script_mut(_script: &mut dyn Script) -> Option<&mut dyn PartialReflect>
    where
        Self: Sized,
    {
        None
    }
}

/// `script` as `&dyn PartialReflect` when its concrete type is `T` (a `TypeId` check through
/// `Any`). The body `#[derive(Script)]` emits for [`ReflectScript::reflect_script`].
pub fn downcast_reflect<T: Script + PartialReflect>(script: &dyn Script) -> Option<&dyn PartialReflect> {
    let any: &dyn std::any::Any = script;
    any.downcast_ref::<T>().map(|value| value as &dyn PartialReflect)
}

/// `script` as `&mut dyn PartialReflect` when its concrete type is `T`. The body
/// `#[derive(Script)]` emits for [`ReflectScript::reflect_script_mut`].
pub fn downcast_reflect_mut<T: Script + PartialReflect>(
    script: &mut dyn Script,
) -> Option<&mut dyn PartialReflect> {
    let any: &mut dyn std::any::Any = script;
    any.downcast_mut::<T>()
        .map(|value| value as &mut dyn PartialReflect)
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// One registered script type: its order and how to build instances.
#[derive(Clone, Copy, Debug)]
pub struct ScriptDesc {
    /// Execution-order group (§ B2 ordering).
    pub order: i32,
    /// Full type path (`std::any::type_name`), the registry key.
    pub type_path: &'static str,
    /// Factory that boxes a default instance.
    pub factory: fn() -> Box<dyn Script>,
    /// [`ReflectScript::reflect_script`] of the registered type: the reflected fields of an
    /// instance of exactly this type (hot reload snapshots through it).
    pub reflect: fn(&dyn Script) -> Option<&dyn PartialReflect>,
    /// [`ReflectScript::reflect_script_mut`] of the registered type (hot reload restores
    /// through it).
    pub reflect_mut: fn(&mut dyn Script) -> Option<&mut dyn PartialReflect>,
}

/// All script types the game registered (via [`RegisterScriptExt::register_script`]).
///
/// The sim bridge uses it to attach the presentation scripts a `KindRegistry` entry names, so
/// spawning from a snapshot needs no game code (`specs/25` § 3).
///
/// Each entry records the [`TypeOwner`] that was current when it was registered (`specs/20`
/// § 3 step 6): hot reload removes a game library's entries with [`ScriptRegistry::remove_owner`]
/// before the new library registers, so no factory into the old library survives the reload.
#[derive(Resource, Debug, Default, Clone)]
pub struct ScriptRegistry {
    entries: BTreeMap<&'static str, ScriptDesc>,
    owners: BTreeMap<&'static str, TypeOwner>,
}

impl ScriptRegistry {
    /// The registration of `type_path`, if the game registered it.
    pub fn get(&self, type_path: &str) -> Option<&ScriptDesc> {
        self.entries.get(type_path)
    }

    /// Every registration, ordered by type path.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &ScriptDesc)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Number of registered script types.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no script type is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Registers `T` under [`TypeOwner::Engine`]. Re-registering the same type path replaces the
    /// entry.
    pub fn register<T: ReflectScript>(&mut self) {
        self.register_owned::<T>(TypeOwner::Engine);
    }

    /// Registers `T` and records `owner` as the registration's owner. Re-registering the same
    /// type path replaces the entry and its owner.
    pub fn register_owned<T: ReflectScript>(&mut self, owner: TypeOwner) {
        let type_path = T::script_type_path();
        self.entries.insert(
            type_path,
            ScriptDesc {
                order: T::order(),
                type_path,
                factory: T::construct,
                reflect: T::reflect_script,
                reflect_mut: T::reflect_script_mut,
            },
        );
        self.owners.insert(type_path, owner);
    }

    /// Who registered `type_path` ([`TypeOwner::Engine`] for [`ScriptRegistry::register`]).
    #[must_use]
    pub fn owner_of(&self, type_path: &str) -> Option<TypeOwner> {
        self.owners.get(type_path).copied()
    }

    /// The type paths `owner` registered, in order.
    #[must_use]
    pub fn type_paths_of(&self, owner: TypeOwner) -> Vec<&'static str> {
        self.owners
            .iter()
            .filter(|(_, o)| **o == owner)
            .map(|(path, _)| *path)
            .collect()
    }

    /// Removes every registration `owner` made and answers how many (hot reload teardown,
    /// `specs/20` § 3 step 6).
    pub fn remove_owner(&mut self, owner: TypeOwner) -> usize {
        let paths = self.type_paths_of(owner);
        for path in &paths {
            self.entries.remove(path);
            self.owners.remove(path);
        }
        paths.len()
    }
}

/// [`App`] extension: [`App::register_script`].
pub trait RegisterScriptExt {
    /// Registers a script type so it can be attached by name (data, the sim bridge, the editor's
    /// "Add Script" list). The type must derive `Script` (or implement [`ReflectScript`] by hand).
    ///
    /// The registration is recorded under the app's current owner ([`App::current_owner`]), so a
    /// hot-reloaded game library's scripts are removed with the library.
    fn register_script<T: ReflectScript>(&mut self) -> &mut Self;
}

impl RegisterScriptExt for App {
    fn register_script<T: ReflectScript>(&mut self) -> &mut Self {
        let owner = self.current_owner();
        self.init_resource::<ScriptRegistry>()
            .world_mut()
            .resource_mut::<ScriptRegistry>()
            .register_owned::<T>(owner);
        self
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// One script attached to an entity: the type path, the instance, and its lifecycle flags.
///
/// `instance` is `None` only while the instance is *taken out* for a call (take-run-reinsert, see
/// the module docs) or after the entity was despawned mid-call.
pub struct ScriptSlot {
    /// Full type path of the script type (`std::any::type_name`).
    pub type_path: String,
    /// Execution-order group; copied from the registration when attached.
    pub order: i32,
    /// The instance, when it is not currently executing.
    pub instance: Option<Box<dyn Script>>,
    /// Whether `start` has run.
    pub started: bool,
    /// Disabled scripts stay attached but are not called.
    pub enabled: bool,
}

impl ScriptSlot {
    /// A slot holding a fresh instance of `T`, with `T`'s registered order.
    pub fn new<T: ReflectScript>() -> Self {
        Self {
            type_path: T::script_type_path().to_string(),
            order: T::order(),
            instance: Some(T::construct()),
            started: false,
            enabled: true,
        }
    }
}

/// The scripts attached to an entity: `Scripts(SmallVec<[ScriptSlot; 2]>)` (§ B2).
///
/// Build one with [`Scripts::with`] and add more with [`Scripts::push`]; systems can
/// `Query<&Scripts>` and read the slots.
#[derive(Component, Default)]
pub struct Scripts(pub SmallVec<[ScriptSlot; 2]>);

impl Scripts {
    /// Scripts holding one instance of `T`.
    pub fn with<T: ReflectScript>() -> Self {
        let mut scripts = Self::default();
        scripts.push::<T>();
        scripts
    }

    /// Appends a slot holding a fresh instance of `T`.
    pub fn push<T: ReflectScript>(&mut self) {
        self.0.push(ScriptSlot::new::<T>());
    }
    /// Appends a slot for `type_path` using `registry`, if the type is registered. Returns false
    /// (and logs a warning once) when it is not — user data must never panic.
    pub fn push_registered(&mut self, type_path: &str, registry: &ScriptRegistry) -> bool {
        match registry.get(type_path) {
            Some(desc) => {
                self.0.push(ScriptSlot {
                    type_path: desc.type_path.to_string(),
                    order: desc.order,
                    instance: Some((desc.factory)()),
                    started: false,
                    enabled: true,
                });
                true
            }
            None => {
                tracing::warn!(target: "regia_framework", "script type {type_path} is not registered");
                false
            }
        }
    }
}

impl FromIterator<ScriptSlot> for Scripts {
    fn from_iter<T: IntoIterator<Item = ScriptSlot>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// Events and messages
// ---------------------------------------------------------------------------

/// A runtime UI event as scripts see it (`specs/18` § 4, ADR-7070): the kind and its small payload.
/// Text payloads stay on the widget (read its `TextInput` or `Text`), so the event stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScriptUiEvent {
    /// Activated (`click_count` 2 for the second click of a double click).
    Click {
        /// Consecutive clicks on the node.
        click_count: u8,
    },
    /// Second click within the double-click window (sent after its `Click`).
    DoubleClick,
    /// Secondary button released over the node it was pressed on.
    ContextClick,
    /// Middle button released over the node it was pressed on.
    MiddleClick,
    /// The pointer entered the node (target only, does not bubble).
    PointerEnter,
    /// The pointer left the node (target only, does not bubble).
    PointerLeave,
    /// A drag started on the node.
    DragStart,
    /// A drag moved (delta in physical px).
    Drag([f32; 2]),
    /// A drag ended.
    DragEnd,
    /// A widget's value changed.
    ValueChanged(ScriptUiValue),
    /// Text was submitted (the text is on the widget).
    Submitted,
    /// Focus moved to (`true`) or from the node.
    FocusChanged(bool),
    /// Wheel or trackpad scroll over the node (physical px).
    Scroll([f32; 2]),
    /// A long press fired.
    LongPress,
    /// A repeat-on-hold fired (the repeat number, from 1).
    Repeat(u32),
}

/// The value of a [`ScriptUiEvent::ValueChanged`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScriptUiValue {
    /// No payload.
    None,
    /// A toggle's state.
    Bool(bool),
    /// A slider's or a progress bar's value.
    Number(f32),
    /// An index into a list of options (dropdown, tab bar).
    Index(usize),
    /// Text changed (the text is on the widget).
    Text,
}

/// Gameplay events delivered to scripts (§ B2 `ScriptEvent` sources).
///
/// Physics messages (`regia_physics::events`, `specs/15` § 3) are forwarded by
/// [`ScriptPlugin`]'s bridge system: trigger events reach the scripts on the trigger entity
/// ([`ScriptEvent::TriggerEnter`] below), collisions and contact forces reach both participants. Queued in the
/// [`Update`] pass, after messages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScriptEvent {
    /// Two colliders began touching. Delivered to scripts on both entities.
    CollisionStarted {
        /// One participant; each side receives the event with itself as `entity`.
        entity: Entity,
        /// The other participant.
        other: Entity,
    },
    /// Two colliders stopped touching. Delivered to scripts on both entities.
    CollisionEnded {
        /// One participant; each side receives the event with itself as `entity`.
        entity: Entity,
        /// The other participant.
        other: Entity,
    },
    /// An entity entered this trigger volume. Delivered to scripts on the trigger entity.
    TriggerEnter {
        /// The entity owning the sensor collider that was entered.
        entity: Entity,
        /// The entering entity.
        other: Entity,
    },
    /// An entity left this trigger volume. Delivered to scripts on the trigger entity.
    TriggerExit {
        /// The entity owning the sensor collider that was left.
        entity: Entity,
        /// The leaving entity.
        other: Entity,
    },
    /// A runtime UI event (`specs/18` § 4, ADR-7070). Delivered to the scripts on the node it is
    /// about (`entity == origin`), then, for every kind but hover, to the scripts on each ancestor
    /// in turn, nearest first (`entity` = that ancestor), up to a `UiStopBubble` node.
    Ui {
        /// The entity whose scripts receive this copy.
        entity: Entity,
        /// The UI node the event is about.
        origin: Entity,
        /// What happened.
        event: ScriptUiEvent,
    },
    /// The total contact force between two colliders crossed a collider's
    /// `contact_force_threshold` this step (`regia_physics::ContactForce`, ADR-7233). Delivered
    /// to scripts on both entities, once per step while the force stays above the threshold.
    ContactForce {
        /// One participant; each side receives the event with itself as `entity`.
        entity: Entity,
        /// The other participant.
        other: Entity,
        /// Total force magnitude on the contact pair, in newtons.
        total_force: f32,
    },
}

/// Queue of [`ScriptEvent`]s waiting for the next [`Update`] delivery pass.
///
/// `queue` is broadcast (every script); `targeted` reaches only the scripts on the named
/// entities (physics events use this so a collectible does not see another trigger's events).
#[derive(Resource, Debug, Default)]
pub struct ScriptEvents {
    queue: Vec<ScriptEvent>,
    targeted: Vec<(Vec<Entity>, ScriptEvent)>,
}

/// Queues `event` for delivery to every script in the world (next `Update` pass).
pub fn broadcast_script_event(world: &mut World, event: ScriptEvent) {
    world
        .get_resource_or_insert_with(ScriptEvents::default)
        .queue
        .push(event);
}

/// Queues `event` for delivery to the scripts on `recipients` only (next `Update` pass).
/// Recipients without scripts simply do not receive it.
pub fn send_script_event_to(
    world: &mut World,
    recipients: impl IntoIterator<Item = Entity>,
    event: ScriptEvent,
) {
    let recipients: Vec<Entity> = recipients.into_iter().collect();
    if recipients.is_empty() {
        return;
    }
    world
        .get_resource_or_insert_with(ScriptEvents::default)
        .targeted
        .push((recipients, event));
}

/// Where a queued message goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MessageTarget {
    Entity(Entity),
    Broadcast,
}

/// Delivers a stored, typed message to a script's `on_message` without the queue knowing the
/// type. Public because [`TimerAction::Message`](crate::timers::TimerAction::Message) stores one;
/// construct messages through [`ScriptCtx::send_message`] or
/// [`schedule_message_after`](crate::timers::schedule_message_after) instead of implementing this.
pub trait MessageEnvelope: Send + Sync {
    /// Calls `script.on_message` with the stored message.
    fn deliver(&mut self, script: &mut dyn Script, ctx: &mut ScriptCtx);
    /// The stored message's type path (diagnostics).
    fn type_path(&self) -> &'static str;
}

pub(crate) struct TypedMessage<M: Reflect + Clone + Send + Sync + 'static> {
    pub(crate) message: M,
}

impl<M: Reflect + Clone + Send + Sync + 'static> MessageEnvelope for TypedMessage<M> {
    fn deliver(&mut self, script: &mut dyn Script, ctx: &mut ScriptCtx) {
        script.on_message(ctx, &self.message);
    }

    fn type_path(&self) -> &'static str {
        std::any::type_name::<M>()
    }
}

/// Queued script messages, delivered (take-run-reinsert) after the `update` calls each frame.
#[derive(Resource, Default)]
pub struct ScriptMessages {
    pub(crate) queue: Vec<(MessageTarget, Box<dyn MessageEnvelope>)>,
}

// ---------------------------------------------------------------------------
// ScriptCtx
// ---------------------------------------------------------------------------

/// A copy of the clock values a script commonly needs (a resource borrow cannot outlive the call).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeInfo {
    /// Delta of the current frame in seconds.
    pub delta_secs: f32,
    /// Total gameplay time in seconds.
    pub elapsed_secs: f32,
    /// Frames run since startup.
    pub frame_count: u64,
}

/// What a script call sees: the entity it is attached to and the whole world (§ B2).
///
/// The thin wrappers cover the common cases; [`ScriptCtx::world`] is the escape hatch. Anything
/// mutated through the context is immediately visible — the caller holds exclusive access.
pub struct ScriptCtx<'w> {
    world: &'w mut World,
    entity: Entity,
}

impl<'w> ScriptCtx<'w> {
    /// Wraps `world` and `entity`. Normally built by the script systems; public for tests and for
    /// hosts that run scripts manually.
    pub fn new(world: &'w mut World, entity: Entity) -> Self {
        Self { world, entity }
    }

    /// The entity this script is attached to.
    pub fn entity(&self) -> Entity {
        self.entity
    }

    /// The full world (escape hatch).
    pub fn world(&mut self) -> &mut World {
        self.world
    }

    /// A shared borrow of a component on this entity.
    pub fn get<C: Component>(&self) -> Option<&C> {
        self.world.get::<C>(self.entity)
    }

    /// An exclusive borrow of a component on this entity.
    pub fn get_mut<C: Component<Mutability = Mutable>>(&mut self) -> Option<Mut<'_, C>> {
        self.world.get_mut::<C>(self.entity)
    }

    /// Inserts a component on this entity.
    pub fn insert<C: Component>(&mut self, component: C) -> &mut Self {
        self.world.entity_mut(self.entity).insert(component);
        self
    }

    /// Removes a component from this entity.
    pub fn remove<C: Component>(&mut self) -> &mut Self {
        self.world.entity_mut(self.entity).remove::<C>();
        self
    }

    /// Exclusive access to this entity's [`Transform`].
    pub fn transform_mut(&mut self) -> Option<Mut<'_, Transform>> {
        self.world.get_mut::<Transform>(self.entity)
    }

    /// Spawns a new entity from `bundle`; returns its id.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        self.world.spawn(bundle).id()
    }

    /// Despawns this script's entity. The running instance is *not* reinserted; after the call
    /// returns it is dropped once [`Script::on_destroy`] has run.
    pub fn despawn(&mut self) {
        self.world.despawn(self.entity);
    }

    /// First entity named `name`, in entity (spawn) order. Names are the `Name` component.
    pub fn find_by_name(&mut self, name: &str) -> Option<Entity> {
        let mut query = self.world.query_filtered::<(Entity, &Name), ()>();
        query
            .iter(self.world)
            .find(|(_, n)| n.as_str() == name)
            .map(|(entity, _)| entity)
    }

    /// Runs `f` with `&mut S` — the script of type `S` attached to `entity` — take-run-reinsert,
    /// the same semantics as the scheduler (the instance leaves its slot for the call).
    ///
    /// `f` also receives the world, because the script instance alone is not enough for most work.
    /// Returns `None` when `entity` has no enabled `S` script.
    pub fn with_script<S: Script, R>(
        &mut self,
        entity: Entity,
        f: impl FnOnce(&mut S, &mut World) -> R,
    ) -> Option<R> {
        run_one_typed::<S, R>(self.world, entity, f)
    }

    /// Queues `message` for `target`. Delivery runs once, after every `update` call of this
    /// `Update` pass. A message sent from `on_message`, `on_event`, or `late_update` waits until
    /// the next `Update`. `M` must be `Reflect + Clone` (§ B6 messages).
    pub fn send_message<M: Reflect + Clone + Send + Sync + 'static>(&mut self, target: Entity, message: M) {
        self.queue_message(MessageTarget::Entity(target), message);
    }

    /// Queues `message` for every entity with scripts.
    pub fn broadcast<M: Reflect + Clone + Send + Sync + 'static>(&mut self, message: M) {
        self.queue_message(MessageTarget::Broadcast, message);
    }

    fn queue_message<M: Reflect + Clone + Send + Sync + 'static>(
        &mut self,
        target: MessageTarget,
        message: M,
    ) {
        self.world
            .get_resource_or_insert_with(ScriptMessages::default)
            .queue
            .push((target, Box::new(TypedMessage { message })));
    }

    /// Copies of the clock values for this frame.
    pub fn time(&self) -> TimeInfo {
        if let Some(time) = self.world.get_resource::<Time>() {
            TimeInfo {
                delta_secs: time.delta_secs(),
                elapsed_secs: time.elapsed_secs(),
                frame_count: time.frame_count(),
            }
        } else {
            TimeInfo {
                delta_secs: 0.0,
                elapsed_secs: 0.0,
                frame_count: 0,
            }
        }
    }

    /// Convenience: the current frame's delta in seconds.
    pub fn delta_secs(&self) -> f32 {
        self.time().delta_secs
    }

    /// The 2D value of input action `name`, when the [`Actions`](regia_input::Actions) resource
    /// exists (added by the input plugin). Unknown actions read as zero.
    pub fn action2d(&self, name: &str) -> Vec2 {
        self.world
            .get_resource::<regia_input::Actions>()
            .map_or(Vec2::ZERO, |a| a.axis2d(name))
    }

    /// Whether input action `name` is active this frame.
    pub fn action_pressed(&self, name: &str) -> bool {
        self.world
            .get_resource::<regia_input::Actions>()
            .is_some_and(|a| a.pressed(name))
    }

    /// Schedules `callback` in `secs` game-seconds, bound to this entity (cancelled if it is
    /// despawned). See [`TimerHub`](crate::timers::TimerHub).
    pub fn after(&mut self, secs: f32, callback: impl FnMut(&mut World) + Send + Sync + 'static) {
        crate::timers::schedule_after(self.world, secs, Some(self.entity), callback);
    }

    /// Schedules a repeating `callback` every `secs` game-seconds, bound to this entity.
    pub fn every(&mut self, secs: f32, callback: impl FnMut(&mut World) + Send + Sync + 'static) {
        crate::timers::schedule_every(self.world, secs, Some(self.entity), callback);
    }

    /// Starts a coroutine bound to this entity (cancelled if it is despawned). See
    /// [`coroutine`](crate::coroutine) for the waits and world access a [`Co`](crate::coroutine::Co)
    /// offers; coroutines are not saved and do not survive a hot reload.
    pub fn start_coroutine<T, F, Fut>(&mut self, body: F) -> crate::coroutine::Coroutine<T>
    where
        T: Send + 'static,
        F: FnOnce(crate::coroutine::Co) -> Fut,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        crate::coroutine::start_coroutine(self.world, Some(self.entity), body)
    }

    /// Queues a message in `secs` game-seconds (see [`ScriptCtx::send_message`]); the timer is
    /// bound to this entity.
    pub fn send_message_in<M: Reflect + Clone + Send + Sync + 'static>(
        &mut self,
        secs: f32,
        target: Entity,
        message: M,
    ) {
        crate::timers::schedule_message_after(self.world, secs, Some(self.entity), target, message);
    }

    /// A deterministic random f32 from the world's [`ScriptRng`] resource (seeded once per app).
    pub fn rand_f32(&mut self) -> f32 {
        self.world
            .get_resource_or_insert_with(ScriptRng::default)
            .rng
            .f32()
    }

    /// A deterministic random sample from `range` (see [`ScriptCtx::rand_f32`]).
    pub fn rand_range<T, R: regia_core::math::SampleRange<T>>(&mut self, range: R) -> T {
        self.world
            .get_resource_or_insert_with(ScriptRng::default)
            .rng
            .range(range)
    }

    /// Deferred commands, applied at the next sync point (`world.commands()`).
    pub fn commands(&mut self) -> Commands<'_, '_> {
        self.world.commands()
    }

    /// Physics queries (`raycast`, `raycast_all`, `shapecast`, `overlap`, `closest_point`) over
    /// the world's [`regia_physics::PhysicsWorld`], filtered by its
    /// [`regia_physics::PhysicsLayers`] query matrix when the game has one (`specs/15` § 3).
    /// `None` when the world has no physics.
    pub fn physics(&self) -> Option<regia_physics::queries::WorldQueries<'_>> {
        let world = self.world.get_resource::<regia_physics::PhysicsWorld>()?;
        Some(regia_physics::queries::WorldQueries {
            world,
            layers: self.world.get_resource::<regia_physics::PhysicsLayers>(),
        })
    }

    /// Teleports this script's entity to `pose` (its `Transform`) on the next fixed step, keeping
    /// or clearing its velocity, without sweeping the way there (`regia_physics::teleport`,
    /// ADR-7233). `false` when the entity has no `RigidBody`.
    pub fn teleport(&mut self, pose: Transform, velocity: regia_physics::TeleportVelocity) -> bool {
        regia_physics::teleport(self.world, self.entity, pose, velocity)
    }
}

/// The shared, deterministic random source scripts read through [`ScriptCtx::rand_f32`].
///
/// Seeded with a fixed seed so replays of the same session match; a game that wants per-run
/// entropy replaces the resource. Presentation may be nondeterministic (`specs/25` § 3), so this
/// is a convenience, not an authority.
#[derive(Resource, Debug)]
pub struct ScriptRng {
    /// The generator.
    pub rng: regia_core::math::Rng,
}

impl Default for ScriptRng {
    fn default() -> Self {
        Self {
            rng: regia_core::math::Rng::new(0x5EED_0000_00C0_FFEE),
        }
    }
}

// ---------------------------------------------------------------------------
// Execution: take, run, reinsert
// ---------------------------------------------------------------------------

/// Which method a pass calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Fixed,
    Update,
    Late,
}

impl Pass {
    fn call(self, script: &mut dyn Script, ctx: &mut ScriptCtx) {
        match self {
            Pass::Fixed => script.fixed_update(ctx),
            Pass::Update => script.update(ctx),
            Pass::Late => script.late_update(ctx),
        }
    }
}

/// Runs one script instance with take-run-reinsert semantics.
///
/// `slot_index`/`type_path` identify the slot the instance came from. This is the heart of § B2:
/// the box leaves the slot (no archetype change), the method mutates the world freely (the caller
/// holds it exclusively), and the instance returns only if its slot still exists.
fn call_one(
    world: &mut World,
    entity: Entity,
    slot_index: usize,
    type_path: &str,
    pass: Pass,
    mut script: Box<dyn Script>,
) {
    // The slot vanished while the instance was out (its entity was despawned by an earlier
    // script, or the slot was disabled/removed): destroy instead of running.
    if !should_run(world, entity, slot_index, type_path) {
        let mut ctx = ScriptCtx::new(world, entity);
        script.on_destroy(&mut ctx);
        return;
    }

    // `start` runs once, immediately before the first call of any pass.
    let needs_start = world
        .get::<Scripts>(entity)
        .is_some_and(|s| s.0.get(slot_index).is_some_and(|slot| !slot.started));
    if needs_start {
        {
            let mut ctx = ScriptCtx::new(world, entity);
            script.start(&mut ctx);
        }
        if let Some(mut scripts) = world.get_mut::<Scripts>(entity)
            && let Some(slot) = scripts.0.get_mut(slot_index)
        {
            slot.started = true;
        }
    }

    {
        let mut ctx = ScriptCtx::new(world, entity);
        pass.call(script.as_mut(), &mut ctx);
    }

    if let Err(mut script) = reinsert(world, entity, slot_index, type_path, script) {
        // The slot is gone (despawned mid-call, disabled, removed): run on_destroy, drop.
        let mut ctx = ScriptCtx::new(world, entity);
        script.on_destroy(&mut ctx);
    }
}

/// Puts `script` back into its slot. `Err(script)` means the slot no longer exists — the caller
/// runs `on_destroy` and drops the instance.
fn reinsert(
    world: &mut World,
    entity: Entity,
    slot_index: usize,
    type_path: &str,
    script: Box<dyn Script>,
) -> Result<(), Box<dyn Script>> {
    if !should_run(world, entity, slot_index, type_path) {
        return Err(script);
    }
    let Some(mut scripts) = world.get_mut::<Scripts>(entity) else {
        return Err(script);
    };
    let Some(slot) = scripts.0.get_mut(slot_index) else {
        return Err(script);
    };
    slot.instance = Some(script);
    Ok(())
}

/// Whether the slot at `slot_index` still exists, is enabled, and matches `type_path`.
fn should_run(world: &World, entity: Entity, slot_index: usize, type_path: &str) -> bool {
    world.get::<Scripts>(entity).is_some_and(|s| {
        s.0.get(slot_index)
            .is_some_and(|slot| slot.enabled && slot.type_path == type_path)
    })
}

/// Nested take-run-reinsert for [`ScriptCtx::with_script`]: downcasts to `S` via trait upcasting
/// (`dyn Script` → `dyn Any`, stable since Rust 1.86).
fn run_one_typed<S: Script, R>(
    world: &mut World,
    entity: Entity,
    f: impl FnOnce(&mut S, &mut World) -> R,
) -> Option<R> {
    // Locate the first enabled slot whose instance is an `S`.
    let mut found: Option<(usize, String)> = None;
    if let Some(scripts) = world.get::<Scripts>(entity) {
        for (index, slot) in scripts.0.iter().enumerate() {
            let is_s = slot.instance.as_deref().is_some_and(|script| {
                let any: &dyn std::any::Any = script;
                any.downcast_ref::<S>().is_some()
            });
            if slot.enabled && is_s {
                found = Some((index, slot.type_path.clone()));
                break;
            }
        }
    }
    let (index, type_path) = found?;

    // Take the box out of the slot.
    let instance = world
        .get_mut::<Scripts>(entity)?
        .0
        .get_mut(index)?
        .instance
        .take()?;

    // Call with `&mut S` over the locally-owned box.
    let mut script = instance;
    let result = {
        let any: &mut dyn std::any::Any = script.as_mut();
        let typed = any
            .downcast_mut::<S>()
            .expect("the probe above checked the concrete type");
        f(typed, world)
    };

    if let Err(mut script) = reinsert(world, entity, index, &type_path, script) {
        let mut ctx = ScriptCtx::new(world, entity);
        script.on_destroy(&mut ctx);
    }
    Some(result)
}

/// The entities a script pass visits: [`Scripts`] holders outside a parked data layer.
type ScriptedQuery = regia_ecs::ecs::query::QueryState<Entity, (With<Scripts>, Without<LayerParked>)>;

/// Scratch for one script pass. Kept across frames so a steady pass does not allocate a
/// `String` per slot, a fresh work list or a fresh query state.
#[derive(Default, Resource)]
struct ScriptPassScratch {
    /// The scripted-entity query, built once. A fresh `QueryState` per pass allocated its
    /// matched-archetype lists again on every pass once a `Scripts` archetype existed (three per
    /// pass, ADR-5554); a kept one only grows when a new archetype matches.
    scripted: Option<ScriptedQuery>,
    /// `(order, entity, slot)` in execution order. The type path is copied into `path` at the call.
    work: Vec<(i32, Entity, usize)>,
    /// Entities with scripts, in spawn order.
    entities: Vec<Entity>,
    /// Enabled slot indexes of the entity being delivered to.
    indices: Vec<usize>,
    /// Type path of the script about to run. Grows only when a longer path appears.
    path: String,
}

/// Collects the (order, entity, slot) work list in § B2's order: execution order groups, then
/// entity spawn order, then slot order. The type path stays on the slot until the call.
fn collect_work(world: &mut World, scratch: &mut ScriptPassScratch) {
    scripted_in_spawn_order(world, &mut scratch.scripted, &mut scratch.entities);
    scratch.work.clear();
    for entity_index in 0..scratch.entities.len() {
        let entity = scratch.entities[entity_index];
        let Some(scripts) = world.get::<Scripts>(entity) else {
            continue;
        };
        for (index, slot) in scripts.0.iter().enumerate() {
            if slot.enabled && slot.instance.is_some() {
                scratch.work.push((slot.order, entity, index));
            }
        }
    }
    // Stable: inside a group, the spawn order and slot order pushed above are kept.
    // `i32` is `Copy`, so the sort does not allocate a key vec.
    scratch.work.sort_by_key(|item| item.0);
}

/// Every entity with [`Scripts`], in the order its `Scripts` arrived in the world
/// (`regia_ecs::spawn_order`, ADR-1347). Not `Entity` order: `Commands::spawn` reserves ids
/// when the command is recorded, so parallel spawning systems get them in thread-timing order.
///
/// `query` is the pass scratch's kept query state, created here on first use.
fn scripted_in_spawn_order(world: &mut World, query: &mut Option<ScriptedQuery>, entities: &mut Vec<Entity>) {
    let state =
        query.get_or_insert_with(|| world.query_filtered::<Entity, (With<Scripts>, Without<LayerParked>)>());
    entities.clear();
    entities.extend(state.iter(world));
    let fallback = regia_ecs::spawn_order::SpawnOrder::<Scripts>::default();
    let order = world
        .get_resource::<regia_ecs::spawn_order::SpawnOrder<Scripts>>()
        .unwrap_or(&fallback);
    order.sort_by_spawn(entities, |entity| *entity);
}

/// Copies the slot's type path into the scratch and reports whether the slot should run.
fn prepare_slot(world: &World, scratch: &mut ScriptPassScratch, entity: Entity, index: usize) -> bool {
    let Some(scripts) = world.get::<Scripts>(entity) else {
        return false;
    };
    let Some(slot) = scripts.0.get(index) else {
        return false;
    };
    if !(slot.enabled && slot.instance.is_some()) {
        return false;
    }
    scratch.path.clone_from(&slot.type_path);
    true
}

/// Enabled slot indexes of `entity` that still hold an instance.
fn fill_slot_indices(world: &World, entity: Entity, indices: &mut Vec<usize>) {
    indices.clear();
    let Some(scripts) = world.get::<Scripts>(entity) else {
        return;
    };
    for (index, slot) in scripts.0.iter().enumerate() {
        if slot.enabled && slot.instance.is_some() {
            indices.push(index);
        }
    }
}

/// The exclusive script pass: calls, then message delivery, then event delivery.
fn run_pass(world: &mut World, pass: Pass) {
    let mut scratch = world.remove_resource::<ScriptPassScratch>().unwrap_or_default();
    collect_work(world, &mut scratch);
    let count = scratch.work.len();
    for slot in 0..count {
        let (_, entity, index) = scratch.work[slot];
        if !prepare_slot(world, &mut scratch, entity, index) {
            continue;
        }
        // The world changed since collection (earlier scripts); re-check and take.
        let instance = (|| {
            let mut scripts = world.get_mut::<Scripts>(entity)?;
            let slot = scripts.0.get_mut(index)?;
            (slot.enabled && slot.instance.is_some()).then(|| slot.instance.take())?
        })();
        if let Some(script) = instance {
            call_one(world, entity, index, &scratch.path, pass, script);
        }
    }

    if pass == Pass::Update {
        deliver_messages(world, &mut scratch);
        deliver_events(world, &mut scratch);
    }
    world.insert_resource(scratch);
}

/// Delivers queued messages, take-run-reinsert, in queue order; broadcast reaches every script
/// entity in spawn order.
fn deliver_messages(world: &mut World, scratch: &mut ScriptPassScratch) {
    let Some(mut messages) = world.remove_resource::<ScriptMessages>() else {
        return;
    };
    let queue = std::mem::take(&mut messages.queue);
    world.insert_resource(messages); // keep flags/identity for anyone holding a handle

    for (target, mut envelope) in queue {
        match target {
            MessageTarget::Entity(entity) => {
                scratch.entities.clear();
                scratch.entities.push(entity);
            }
            MessageTarget::Broadcast => {
                scripted_in_spawn_order(world, &mut scratch.scripted, &mut scratch.entities);
            }
        }
        tracing::trace!(target: "regia_framework", "delivering script message {}", envelope.type_path());
        let entity_count = scratch.entities.len();
        for entity_index in 0..entity_count {
            let entity = scratch.entities[entity_index];
            // A member of a parked data layer runs nothing, a message addressed to it included.
            if world.get::<LayerParked>(entity).is_some() {
                continue;
            }
            fill_slot_indices(world, entity, &mut scratch.indices);
            let slot_count = scratch.indices.len();
            for slot_index in 0..slot_count {
                let index = scratch.indices[slot_index];
                if !prepare_slot(world, scratch, entity, index) {
                    continue;
                }
                let Some(instance) = (|| {
                    let mut scripts = world.get_mut::<Scripts>(entity)?;
                    scripts.0.get_mut(index)?.instance.take()
                })() else {
                    continue;
                };
                call_with(envelope.as_mut(), world, entity, index, &scratch.path, instance);
            }
        }
    }
}

/// One `on_message` call with the same reinsertion rules as [`call_one`].
fn call_with(
    envelope: &mut dyn MessageEnvelope,
    world: &mut World,
    entity: Entity,
    index: usize,
    type_path: &str,
    mut script: Box<dyn Script>,
) {
    let mut ctx = ScriptCtx::new(world, entity);
    envelope.deliver(script.as_mut(), &mut ctx);
    if let Err(mut script) = reinsert(world, entity, index, type_path, script) {
        let mut ctx = ScriptCtx::new(world, entity);
        script.on_destroy(&mut ctx);
    }
}

/// Broadcasts queued [`ScriptEvent`]s to every script entity, then delivers the targeted
/// events to their recipients' scripts only.
fn deliver_events(world: &mut World, scratch: &mut ScriptPassScratch) {
    let Some(mut events) = world.remove_resource::<ScriptEvents>() else {
        return;
    };
    let queue = std::mem::take(&mut events.queue);
    let targeted = std::mem::take(&mut events.targeted);
    world.insert_resource(events);

    for event in queue {
        scripted_in_spawn_order(world, &mut scratch.scripted, &mut scratch.entities);
        deliver_event_to_entities(world, scratch, event);
    }
    for (recipients, event) in targeted {
        // Only entities that still exist and carry scripts receive the event.
        scratch.entities.clear();
        for entity in recipients {
            if world.get::<Scripts>(entity).is_some() && world.get::<LayerParked>(entity).is_none() {
                scratch.entities.push(entity);
            }
        }
        deliver_event_to_entities(world, scratch, event);
    }
}

/// Runs the scripts of each entity in `scratch.entities` with `event` (take-run-reinsert).
fn deliver_event_to_entities(world: &mut World, scratch: &mut ScriptPassScratch, event: ScriptEvent) {
    let entity_count = scratch.entities.len();
    for entity_index in 0..entity_count {
        let entity = scratch.entities[entity_index];
        fill_slot_indices(world, entity, &mut scratch.indices);
        let slot_count = scratch.indices.len();
        for slot_index in 0..slot_count {
            let index = scratch.indices[slot_index];
            if !prepare_slot(world, scratch, entity, index) {
                continue;
            }
            let Some(instance) = (|| {
                let mut scripts = world.get_mut::<Scripts>(entity)?;
                scripts.0.get_mut(index)?.instance.take()
            })() else {
                continue;
            };
            let mut ctx = ScriptCtx::new(world, entity);
            let mut script = instance;
            script.on_event(&mut ctx, &event);
            if let Err(mut script) = reinsert(world, entity, index, &scratch.path, script) {
                let mut ctx = ScriptCtx::new(world, entity);
                script.on_destroy(&mut ctx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin and systems
// ---------------------------------------------------------------------------

/// Registers the script storage, registry and messages, and runs the three per-schedule passes
/// plus the physics event bridge (`specs/15` § 3 → `specs/14` § B2).
pub struct ScriptPlugin;

impl Plugin for ScriptPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ScriptRegistry>()
            .init_resource::<ScriptMessages>()
            .init_resource::<ScriptEvents>()
            .init_resource::<PhysicsEventCursors>()
            .add_systems(FixedUpdate, run_scripts_fixed.in_set(ScriptsSet::FixedUpdate))
            .add_systems(
                Update,
                (forward_physics_script_events, run_scripts_update)
                    .chain()
                    .in_set(ScriptsSet::Update),
            )
            .add_systems(
                PostUpdate,
                run_scripts_late
                    .in_set(PostUpdateSet::LateScripts)
                    .in_set(ScriptsSet::Late),
            );
        // UI events reach scripts, and UI bindings read script fields (`specs/18` §§ 4-5, ADR-7070).
        #[cfg(feature = "client")]
        app.init_resource::<UiEventCursors>()
            .insert_resource(regia_ui::ScriptFieldReader(read_script_field))
            .add_systems(
                Update,
                forward_ui_script_events
                    .in_set(ScriptsSet::Update)
                    .before(run_scripts_update),
            );
        // Run order follows the order scripts arrived, not entity ids (ADR-1347).
        regia_ecs::spawn_order::track_spawn_order::<Scripts>(app.world_mut());
    }
}

/// Persistent read cursors for the physics message streams (one per forwarded event kind).
#[derive(Debug, Default, Resource)]
struct PhysicsEventCursors {
    /// Collisions that began.
    started: regia_ecs::ecs::message::MessageCursor<regia_physics::CollisionStarted>,
    /// Collisions that ended.
    ended: regia_ecs::ecs::message::MessageCursor<regia_physics::CollisionEnded>,
    /// Sensors that were entered.
    trigger_enter: regia_ecs::ecs::message::MessageCursor<regia_physics::TriggerEnter>,
    /// Sensors that were left.
    trigger_exit: regia_ecs::ecs::message::MessageCursor<regia_physics::TriggerExit>,
    /// Contact forces over a collider's threshold (ADR-7233).
    contact_force: regia_ecs::ecs::message::MessageCursor<regia_physics::ContactForce>,
}

/// `Update` (before scripts): forwards `regia_physics` collision, contact-force and trigger
/// messages (`specs/15` § 3) into the [`ScriptEvents`] queue as targeted [`ScriptEvent`]s —
/// trigger events reach the trigger entity's scripts, collisions and contact forces reach both
/// participants. A no-op
/// without `regia_physics::PhysicsPlugin` (the message resources do not exist).
fn forward_physics_script_events(world: &mut World) {
    let Some(mut cursors) = world.remove_resource::<PhysicsEventCursors>() else {
        return;
    };
    // Drain each stream into owned events first: the message borrow must end before the
    // queued events are written back into the world below.
    let started: Vec<_> = world
        .get_resource::<Messages<regia_physics::CollisionStarted>>()
        .map(|messages| cursors.started.read(messages).cloned().collect())
        .unwrap_or_default();
    let ended: Vec<_> = world
        .get_resource::<Messages<regia_physics::CollisionEnded>>()
        .map(|messages| cursors.ended.read(messages).cloned().collect())
        .unwrap_or_default();
    let trigger_enter: Vec<_> = world
        .get_resource::<Messages<regia_physics::TriggerEnter>>()
        .map(|messages| cursors.trigger_enter.read(messages).cloned().collect())
        .unwrap_or_default();
    let trigger_exit: Vec<_> = world
        .get_resource::<Messages<regia_physics::TriggerExit>>()
        .map(|messages| cursors.trigger_exit.read(messages).cloned().collect())
        .unwrap_or_default();
    let contact_force: Vec<_> = world
        .get_resource::<Messages<regia_physics::ContactForce>>()
        .map(|messages| cursors.contact_force.read(messages).cloned().collect())
        .unwrap_or_default();
    world.insert_resource(cursors);

    // Collisions: each participant sees the event with itself as `entity`.
    for event in started {
        send_script_event_to(
            world,
            [event.entity_a],
            ScriptEvent::CollisionStarted {
                entity: event.entity_a,
                other: event.entity_b,
            },
        );
        send_script_event_to(
            world,
            [event.entity_b],
            ScriptEvent::CollisionStarted {
                entity: event.entity_b,
                other: event.entity_a,
            },
        );
    }
    for event in ended {
        send_script_event_to(
            world,
            [event.entity_a],
            ScriptEvent::CollisionEnded {
                entity: event.entity_a,
                other: event.entity_b,
            },
        );
        send_script_event_to(
            world,
            [event.entity_b],
            ScriptEvent::CollisionEnded {
                entity: event.entity_b,
                other: event.entity_a,
            },
        );
    }
    // Contact forces: like collisions, each participant sees itself as `entity` (ADR-7233).
    for event in contact_force {
        for (entity, other) in [(event.entity_a, event.entity_b), (event.entity_b, event.entity_a)] {
            send_script_event_to(
                world,
                [entity],
                ScriptEvent::ContactForce {
                    entity,
                    other,
                    total_force: event.total_force,
                },
            );
        }
    }
    // Triggers: only the trigger's scripts see the event.
    for event in trigger_enter {
        send_script_event_to(
            world,
            [event.trigger],
            ScriptEvent::TriggerEnter {
                entity: event.trigger,
                other: event.other,
            },
        );
    }
    for event in trigger_exit {
        send_script_event_to(
            world,
            [event.trigger],
            ScriptEvent::TriggerExit {
                entity: event.trigger,
                other: event.other,
            },
        );
    }
}

/// Persistent read cursors for the UI event streams (`specs/18` § 4).
#[cfg(feature = "client")]
#[derive(Default, Resource)]
struct UiEventCursors {
    /// Events on their target.
    events: regia_ecs::ecs::message::MessageCursor<regia_ui::UiEvent>,
    /// Bubbled copies on ancestors.
    bubbled: regia_ecs::ecs::message::MessageCursor<regia_ui::UiBubbledEvent>,
}

/// The script-side form of a UI event kind.
#[cfg(feature = "client")]
fn script_ui_event(event: &regia_ui::UiEvent) -> ScriptUiEvent {
    use regia_ui::{UiEventKind as K, UiValue as V};
    match &event.kind {
        K::Click => ScriptUiEvent::Click {
            click_count: event.click_count,
        },
        K::DoubleClick => ScriptUiEvent::DoubleClick,
        K::ContextClick => ScriptUiEvent::ContextClick,
        K::MiddleClick => ScriptUiEvent::MiddleClick,
        K::PointerEnter => ScriptUiEvent::PointerEnter,
        K::PointerLeave => ScriptUiEvent::PointerLeave,
        K::DragStart => ScriptUiEvent::DragStart,
        K::Drag(delta) => ScriptUiEvent::Drag(*delta),
        K::DragEnd => ScriptUiEvent::DragEnd,
        K::ValueChanged(value) => ScriptUiEvent::ValueChanged(match value {
            V::None => ScriptUiValue::None,
            V::Bool(b) => ScriptUiValue::Bool(*b),
            V::Number(n) => ScriptUiValue::Number(*n),
            V::Index(i) => ScriptUiValue::Index(*i),
            V::Text(_) => ScriptUiValue::Text,
        }),
        K::Submitted(_) => ScriptUiEvent::Submitted,
        K::FocusChanged(gained) => ScriptUiEvent::FocusChanged(*gained),
        K::Scroll(delta) => ScriptUiEvent::Scroll(*delta),
        K::LongPress => ScriptUiEvent::LongPress,
        K::Repeat(n) => ScriptUiEvent::Repeat(*n),
    }
}

/// `Update` (before scripts): queues this frame's UI events for the scripts on their target and
/// the bubbled copies for the scripts on each ancestor (`specs/18` § 4, ADR-7070). Events the UI
/// writes after the scripts ran arrive on the next frame's pass (the cursors keep their place).
/// Entities without scripts cost a lookup and nothing else.
#[cfg(feature = "client")]
fn forward_ui_script_events(world: &mut World) {
    let _span = tracing::trace_span!("ui_script_events").entered();
    let Some(mut cursors) = world.remove_resource::<UiEventCursors>() else {
        return;
    };
    let on_target: Vec<(Entity, ScriptUiEvent)> = world
        .get_resource::<Messages<regia_ui::UiEvent>>()
        .map(|messages| {
            cursors
                .events
                .read(messages)
                .map(|e| (e.target, script_ui_event(e)))
                .collect()
        })
        .unwrap_or_default();
    let bubbled: Vec<(Entity, Entity, ScriptUiEvent)> = world
        .get_resource::<Messages<regia_ui::UiBubbledEvent>>()
        .map(|messages| {
            cursors
                .bubbled
                .read(messages)
                .map(|b| (b.current, b.event.target, script_ui_event(&b.event)))
                .collect()
        })
        .unwrap_or_default();
    world.insert_resource(cursors);
    for (target, event) in on_target {
        if world.get::<Scripts>(target).is_some() {
            send_script_event_to(
                world,
                [target],
                ScriptEvent::Ui {
                    entity: target,
                    origin: target,
                    event,
                },
            );
        }
    }
    for (handler, origin, event) in bubbled {
        if world.get::<Scripts>(handler).is_some() {
            send_script_event_to(
                world,
                [handler],
                ScriptEvent::Ui {
                    entity: handler,
                    origin,
                    event,
                },
            );
        }
    }
}

/// The short name of a script type path: `game::hud::Hud<T>` → `Hud`.
#[cfg(feature = "client")]
fn short_script_name(type_path: &str) -> &str {
    let base = type_path.split('<').next().unwrap_or(type_path);
    base.rsplit("::").next().unwrap_or(base)
}

/// The [`regia_ui::ScriptFieldReader`] the plugin installs: the field `path` of the first script
/// named `script` (short type name) on `entity` or its nearest ancestor that has one.
#[cfg(feature = "client")]
fn read_script_field<'w>(
    world: &'w World,
    entity: Entity,
    script: &str,
    path: &str,
) -> Result<&'w dyn regia_ecs::reflect::PartialReflect, String> {
    let mut current = entity;
    for _ in 0..4096 {
        if let Some(scripts) = world.get::<Scripts>(current)
            && let Some(slot) = scripts
                .0
                .iter()
                .find(|slot| short_script_name(&slot.type_path) == script)
        {
            let Some(instance) = slot.instance.as_deref() else {
                return Err(format!("script {script:?} on {current:?} is running"));
            };
            let Some(fields) = instance.reflect_fields() else {
                return Err(format!(
                    "script {script:?} exposes no fields (implement Script::reflect_fields)"
                ));
            };
            return regia_ui::binding::resolve_path(fields, path);
        }
        match world.get::<ChildOf>(current) {
            Some(parent) => current = parent.parent(),
            None => break,
        }
    }
    Err(format!("no script {script:?} on {entity:?} or its ancestors"))
}

/// `FixedUpdate`: `Script::fixed_update`, take-run-reinsert.
#[tracing::instrument(skip_all, target = "regia_framework", name = "scripts_fixed")]
fn run_scripts_fixed(world: &mut World) {
    run_pass(world, Pass::Fixed);
}

/// `Update`: `Script::update`, then message and event delivery.
#[tracing::instrument(skip_all, target = "regia_framework", name = "scripts_update")]
fn run_scripts_update(world: &mut World) {
    run_pass(world, Pass::Update);
}

/// `PostUpdate` (before transform propagation): `Script::late_update`.
#[tracing::instrument(skip_all, target = "regia_framework", name = "scripts_late")]
fn run_scripts_late(world: &mut World) {
    run_pass(world, Pass::Late);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Where test scripts record their calls.
    #[derive(Resource, Default, Debug)]
    struct Log(Vec<String>);

    impl Log {
        fn push(world: &mut World, entry: impl Into<String>) {
            world
                .get_resource_or_insert_with(Log::default)
                .0
                .push(entry.into());
        }
    }

    /// A script that records which method ran, with a configurable order.
    #[derive(Reflect, Default, Clone)]
    #[allow(dead_code)]
    struct Recorder {
        name: String,
    }

    impl Script for Recorder {
        fn start(&mut self, ctx: &mut ScriptCtx) {
            Log::push(ctx.world(), format!("{}:start", self.name));
        }

        fn update(&mut self, ctx: &mut ScriptCtx) {
            Log::push(ctx.world(), format!("{}:update", self.name));
        }

        fn late_update(&mut self, ctx: &mut ScriptCtx) {
            Log::push(ctx.world(), format!("{}:late", self.name));
        }

        fn on_destroy(&mut self, ctx: &mut ScriptCtx) {
            Log::push(ctx.world(), format!("{}:destroy", self.name));
        }
    }

    /// Derive-generated registration for `Recorder` with a fixed order (what
    /// `#[derive(Script)]` + `#[script(order = N)]` emits).
    struct Registered<R>(std::marker::PhantomData<R>);

    impl<R: RecorderCtor + Send + Sync + 'static> ReflectScript for Registered<R> {
        fn construct() -> Box<dyn Script>
        where
            Self: Sized,
        {
            Box::new(R::make())
        }
        fn order() -> i32
        where
            Self: Sized,
        {
            R::ORDER
        }
        fn script_type_path() -> &'static str
        where
            Self: Sized,
        {
            R::TYPE_PATH
        }
    }

    trait RecorderCtor {
        const ORDER: i32;
        const TYPE_PATH: &'static str;
        fn make() -> Recorder;
    }

    macro_rules! recorder {
        ($tag:ident, $order:expr, $path:literal, $name:literal) => {
            #[allow(dead_code)]
            struct $tag;
            impl RecorderCtor for $tag {
                const ORDER: i32 = $order;
                const TYPE_PATH: &'static str = $path;
                fn make() -> Recorder {
                    Recorder { name: $name.into() }
                }
            }
        };
    }

    recorder!(Early, -100, "test::Early", "early");
    recorder!(Mid, 0, "test::Mid", "mid");
    recorder!(LateOrder, 50, "test::LateOrder", "late_order");

    fn make_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ScriptPlugin))
            .init_resource::<Log>()
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    fn spawn_with(app: &mut App, scripts: Scripts) -> Entity {
        app.world_mut().spawn(scripts).id()
    }

    #[test]
    fn execution_order_groups_then_spawn_order() {
        // Entity A gets order 50, entity B order -100, entity C order 0: run order must be
        // B (-100), C (0), A (50) regardless of spawn order.
        let mut app = make_app();
        let a = spawn_with(
            &mut app,
            Scripts(
                vec![ScriptSlot {
                    type_path: "test::LateOrder".into(),
                    order: 50,
                    instance: Some(Registered::<LateOrder>::construct()),
                    started: false,
                    enabled: true,
                }]
                .into_iter()
                .collect(),
            ),
        );
        let b = spawn_with(
            &mut app,
            Scripts(
                vec![ScriptSlot {
                    type_path: "test::Early".into(),
                    order: -100,
                    instance: Some(Box::new(Recorder { name: "early".into() })),
                    started: false,
                    enabled: true,
                }]
                .into_iter()
                .collect(),
            ),
        );
        let _c = spawn_with(
            &mut app,
            Scripts(
                vec![ScriptSlot {
                    type_path: "test::Mid".into(),
                    order: 0,
                    instance: Some(Box::new(Recorder { name: "mid".into() })),
                    started: false,
                    enabled: true,
                }]
                .into_iter()
                .collect(),
            ),
        );
        let _ = (a, b);

        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        let order: Vec<&str> = log
            .0
            .iter()
            .filter(|e| e.ends_with(":update"))
            .map(|e| e.strip_suffix(":update").unwrap_or(""))
            .collect();
        assert_eq!(order, vec!["early", "mid", "late_order"], "full log: {:?}", log.0);
    }

    #[test]
    fn a_parked_entity_runs_no_script_and_starts_when_live() {
        use regia_scene::layers::LayerParked;
        use regia_scene::prelude::Visibility;

        /// Records events and messages, which `Recorder` does not.
        #[derive(Reflect, Default)]
        struct Listener;
        impl Script for Listener {
            fn on_event(&mut self, ctx: &mut ScriptCtx, _event: &ScriptEvent) {
                Log::push(ctx.world(), "event");
            }
            fn on_message(&mut self, ctx: &mut ScriptCtx, _message: &dyn Reflect) {
                Log::push(ctx.world(), "message");
            }
        }
        impl ReflectScript for Listener {
            fn construct() -> Box<dyn Script> {
                Box::new(Self)
            }
            fn order() -> i32 {
                0
            }
            fn script_type_path() -> &'static str {
                "test::Listener"
            }
        }

        let mut app = make_app();
        let mut scripts = Scripts::with::<Registered<Mid>>();
        scripts.push::<Listener>();
        let entity = spawn_with(&mut app, scripts);
        app.world_mut().entity_mut(entity).insert(LayerParked {
            restore: Visibility::Inherited,
            inherited: false,
        });
        let event = ScriptEvent::TriggerEnter {
            entity,
            other: entity,
        };
        broadcast_script_event(app.world_mut(), event);
        send_script_event_to(app.world_mut(), [entity], event);
        app.update();
        assert_eq!(app.world().resource::<Log>().0, Vec::<String>::new());

        app.world_mut().entity_mut(entity).remove::<LayerParked>();
        send_script_event_to(app.world_mut(), [entity], event);
        app.update();
        let log = &app.world().resource::<Log>().0;
        assert_eq!(log.first().map(String::as_str), Some("mid:start"));
        assert!(log.iter().any(|entry| entry == "event"), "{log:?}");
    }

    #[test]
    fn start_runs_once_immediately_before_first_update() {
        let mut app = make_app();
        spawn_with(
            &mut app,
            Scripts::from_iter([ScriptSlot {
                type_path: "test::Mid".into(),
                order: 0,
                instance: Some(Box::new(Recorder { name: "mid".into() })),
                started: false,
                enabled: true,
            }]),
        );
        app.update();
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        let starts = log.0.iter().filter(|e| e.ends_with(":start")).count();
        assert_eq!(starts, 1, "start must run exactly once: {:?}", log.0);
        // First frame order: start before update.
        assert_eq!(log.0[0], "mid:start");
        assert_eq!(log.0[1], "mid:update");
    }

    #[test]
    fn update_runs_every_frame_and_late_after_update() {
        let mut app = make_app();
        spawn_with(
            &mut app,
            Scripts::from_iter([ScriptSlot {
                type_path: "test::Mid".into(),
                order: 0,
                instance: Some(Box::new(Recorder { name: "mid".into() })),
                started: false,
                enabled: true,
            }]),
        );
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(log.0, vec!["mid:start", "mid:update", "mid:late"]);
    }

    #[test]
    fn despawn_own_entity_drops_after_on_destroy() {
        // A script that despawns its own entity during update: on_destroy must run, the next
        // script must still execute (no panic, no lost instance), and the entity must be gone.
        #[derive(Reflect, Default, Clone)]
        struct Suicidal {
            ran: bool,
        }
        impl Script for Suicidal {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                if !self.ran {
                    self.ran = true;
                    Log::push(ctx.world(), "suicidal:update");
                    ctx.despawn();
                }
            }
            fn on_destroy(&mut self, ctx: &mut ScriptCtx) {
                Log::push(ctx.world(), "suicidal:destroy");
            }
        }

        let mut app = make_app();
        let entity = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Suicidal".into(),
                order: 0,
                instance: Some(Box::new(Suicidal::default())),
                started: false,
                enabled: true,
            }]))
            .id();
        spawn_with(
            &mut app,
            Scripts::from_iter([ScriptSlot {
                type_path: "test::Mid".into(),
                order: 10,
                instance: Some(Box::new(Recorder { name: "mid".into() })),
                started: false,
                enabled: true,
            }]),
        );

        app.update();
        assert!(
            app.world().get_entity(entity).is_err(),
            "entity must be despawned"
        );
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        // The Update pass first (update, destroy), then the PostUpdate late pass adds "mid:late".
        assert_eq!(
            log.0,
            vec![
                "suicidal:update".to_string(),
                "suicidal:destroy".to_string(),
                "mid:start".to_string(),
                "mid:update".to_string(),
                "mid:late".to_string()
            ]
        );
    }

    #[test]
    fn disabled_slots_are_skipped_until_enabled() {
        let mut app = make_app();
        let entity = spawn_with(
            &mut app,
            Scripts::from_iter([ScriptSlot {
                type_path: "test::Mid".into(),
                order: 0,
                instance: Some(Box::new(Recorder { name: "mid".into() })),
                started: false,
                enabled: false,
            }]),
        );
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert!(log.0.is_empty(), "disabled scripts must not run: {:?}", log.0);

        app.world_mut()
            .get_mut::<Scripts>(entity)
            .expect("scripts component")
            .0[0]
            .enabled = true;
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(
            log.0,
            vec![
                "mid:start".to_string(),
                "mid:update".to_string(),
                "mid:late".to_string()
            ]
        );
    }

    #[test]
    fn messages_are_delivered_same_frame_after_updates() {
        #[derive(Reflect, Default, Clone)]
        struct Ping {
            value: u32,
        }

        #[derive(Reflect, Default, Clone)]
        struct Sender;

        impl Script for Sender {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                let target = ctx.find_by_name("receiver").expect("receiver exists");
                ctx.send_message(target, Ping { value: 7 });
            }
        }

        #[derive(Reflect, Default, Clone)]
        struct Receiver {
            got: Option<u32>,
        }

        impl Script for Receiver {
            fn start(&mut self, ctx: &mut ScriptCtx) {
                ctx.insert(Name::new("receiver"));
            }
            fn on_message(&mut self, ctx: &mut ScriptCtx, message: &dyn Reflect) {
                if let Some(ping) = message.as_partial_reflect().try_downcast_ref::<Ping>() {
                    self.got = Some(ping.value);
                    Log::push(ctx.world(), "receiver:ping");
                }
            }
        }

        let mut app = make_app();
        // Receiver spawns first (lower entity id) — sender's update still runs first (order -10).
        app.world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Receiver".into(),
                order: 0,
                instance: Some(Box::new(Receiver::default())),
                started: false,
                enabled: true,
            }]))
            .insert(Name::new("receiver"));
        app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
            type_path: "test::Sender".into(),
            order: -10,
            instance: Some(Box::new(Sender)),
            started: false,
            enabled: true,
        }]));

        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(
            log.0.last().map(String::as_str),
            Some("receiver:ping"),
            "{:?}",
            log.0
        );
    }

    #[test]
    fn broadcast_reaches_every_script_entity() {
        #[derive(Reflect, Default, Clone)]
        struct Shout;

        #[derive(Reflect, Default, Clone)]
        struct Listener {
            name: String,
        }

        impl Script for Listener {
            fn on_message(&mut self, ctx: &mut ScriptCtx, message: &dyn Reflect) {
                if message.as_partial_reflect().try_downcast_ref::<Shout>().is_some() {
                    Log::push(ctx.world(), format!("{}:shout", self.name));
                }
            }
        }

        let mut app = make_app();
        for name in ["a", "b"] {
            app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Listener".into(),
                order: 0,
                instance: Some(Box::new(Listener { name: name.into() })),
                started: false,
                enabled: true,
            }]));
        }
        // Broadcast from outside the scheduler (the helper a system would use).
        broadcast_script_event(
            app.world_mut(),
            ScriptEvent::TriggerEnter {
                entity: Entity::PLACEHOLDER,
                other: Entity::PLACEHOLDER,
            },
        );
        app.world_mut()
            .get_resource_or_insert_with(ScriptMessages::default)
            .queue
            .push((
                MessageTarget::Broadcast,
                Box::new(TypedMessage { message: Shout }),
            ));

        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        let shouts = log.0.iter().filter(|e| e.ends_with(":shout")).count();
        assert_eq!(shouts, 2, "{:?}", log.0);
    }

    #[test]
    fn events_are_broadcast_to_all_scripts() {
        #[derive(Reflect, Default, Clone)]
        struct Evented;

        impl Script for Evented {
            fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
                if matches!(event, ScriptEvent::TriggerEnter { .. }) {
                    Log::push(ctx.world(), "evented:trigger");
                }
            }
        }

        let mut app = make_app();
        app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
            type_path: "test::Evented".into(),
            order: 0,
            instance: Some(Box::new(Evented)),
            started: false,
            enabled: true,
        }]));
        broadcast_script_event(
            app.world_mut(),
            ScriptEvent::TriggerEnter {
                entity: Entity::PLACEHOLDER,
                other: Entity::PLACEHOLDER,
            },
        );

        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(log.0, vec!["evented:trigger"]);
    }

    #[test]
    fn with_script_downcasts_and_reinserts() {
        #[derive(Reflect, Default, Clone)]
        struct Counter {
            count: u32,
        }

        impl Script for Counter {}

        let mut app = make_app();
        let a = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Counter".into(),
                order: 0,
                instance: Some(Box::new(Counter { count: 41 })),
                started: false,
                enabled: true,
            }]))
            .id();
        let b = app.world_mut().spawn(()).id();

        let got = ScriptCtx::new(app.world_mut(), b).with_script::<Counter, u32>(a, |c, _| {
            c.count += 1;
            c.count
        });
        assert_eq!(got, Some(42));

        // Reinserted: readable again.
        let again = ScriptCtx::new(app.world_mut(), b).with_script::<Counter, u32>(a, |c, _| c.count);
        assert_eq!(again, Some(42));

        // No Counter on b: None, no panic.
        let missing = ScriptCtx::new(app.world_mut(), b).with_script::<Counter, u32>(b, |c, _| c.count);
        assert_eq!(missing, None);
    }

    #[test]
    fn script_mutates_components_through_ctx() {
        #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
        #[reflect(Component, Default)]
        struct Marker(f32);

        #[derive(Reflect, Default, Clone)]
        struct Mutator;

        impl Script for Mutator {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                ctx.insert(Marker(3.5));
            }
        }

        let mut app = make_app();
        app.register_type::<Marker>();
        let entity = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Mutator".into(),
                order: 0,
                instance: Some(Box::new(Mutator)),
                started: false,
                enabled: true,
            }]))
            .id();

        app.update();
        assert_eq!(app.world().get::<Marker>(entity), Some(&Marker(3.5)));
    }

    #[test]
    fn registry_stores_order_and_factory() {
        let mut registry = ScriptRegistry::default();
        registry.register::<Registered<Early>>();
        assert_eq!(registry.get("test::Early").map(|d| d.order), Some(-100));
        assert!(registry.get("test::Missing").is_none());

        // The factory builds a working instance: attach through push_registered and run it.
        let mut app = make_app();
        app.register_script::<Registered<Early>>();
        let registry = app.world().resource::<ScriptRegistry>().clone();
        let entity = app.world_mut().spawn(Scripts::default()).id();
        app.world_mut()
            .get_mut::<Scripts>(entity)
            .expect("scripts component")
            .push_registered("test::Early", &registry);
        assert!(app.world().get::<Scripts>(entity).is_some());

        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert!(log.0.contains(&"early:start".to_string()), "{:?}", log.0);
        assert!(log.0.contains(&"early:update".to_string()), "{:?}", log.0);
    }

    #[test]
    fn fixed_update_runs_on_fixed_steps() {
        #[derive(Reflect, Default, Clone)]
        struct FixedOnly;

        impl Script for FixedOnly {
            fn fixed_update(&mut self, ctx: &mut ScriptCtx) {
                Log::push(ctx.world(), "fixed");
            }
        }

        let mut app = make_app();
        app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
            type_path: "test::FixedOnly".into(),
            order: 0,
            instance: Some(Box::new(FixedOnly)),
            started: false,
            enabled: true,
        }]));

        // An independent count of the fixed steps the app ran: the script must run once per step.
        #[derive(Resource, Default)]
        struct FixedSteps(usize);
        app.init_resource::<FixedSteps>();
        app.add_systems(FixedUpdate, |mut steps: ResMut<FixedSteps>| steps.0 += 1);

        // 16 ms frames at 60 Hz fixed → one FixedUpdate most frames.
        for _ in 0..10 {
            app.update();
        }
        let steps = app.world().resource::<FixedSteps>().0;
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        let fixed_calls = log.0.iter().filter(|e| *e == "fixed").count();
        assert!(fixed_calls >= 1, "fixed_update must run: {log:?}");
        assert_eq!(
            fixed_calls, steps,
            "one fixed_update call per fixed step: {log:?}"
        );
    }

    /// Spec 14 B2: the `ScriptCtx` component, spawn, clock, input, random and command wrappers
    /// do what their docs say on the script's own entity.
    #[test]
    fn script_ctx_wrappers_act_on_the_scripts_entity() {
        use regia_core::math::Vec3;
        #[derive(Component, Debug, PartialEq)]
        struct Hp(u32);
        #[derive(Component, Debug, PartialEq)]
        struct Tag;

        let mut app = make_app();
        app.update(); // the clock has a frame behind it
        let me = app
            .world_mut()
            .spawn((Hp(10), Tag, Transform::from_translation(Vec3::new(1.0, 0.0, 0.0))))
            .id();
        let world = app.world_mut();
        let mut ctx = ScriptCtx::new(world, me);

        ctx.get_mut::<Hp>().expect("hp").0 -= 3;
        assert_eq!(ctx.get::<Hp>(), Some(&Hp(7)));
        ctx.remove::<Tag>();
        assert!(
            ctx.get::<Tag>().is_none(),
            "remove takes the component off this entity"
        );
        ctx.transform_mut().expect("transform").translation.y = 2.0;
        assert_eq!(
            ctx.get::<Transform>().map(|t| t.translation),
            Some(Vec3::new(1.0, 2.0, 0.0))
        );
        let child = ctx.spawn(Hp(1));
        assert_ne!(child, me);
        assert_eq!(ctx.world().get::<Hp>(child), Some(&Hp(1)));

        // The clock: `delta_secs()` is `time().delta_secs`, and both read the world's `Time`.
        let time = ctx.time();
        assert_eq!(ctx.delta_secs(), time.delta_secs);
        let (delta, frames) = {
            let clock = ctx.world().resource::<Time>();
            (clock.delta_secs(), clock.frame_count())
        };
        assert_eq!(time.delta_secs, delta);
        assert_eq!(time.frame_count, frames);

        // Without the input plugin, actions read as zero and not pressed (no panic).
        assert_eq!(ctx.action2d("move"), Vec2::ZERO);
        assert!(!ctx.action_pressed("jump"));
        ctx.world().init_resource::<regia_input::Actions>();
        assert_eq!(
            ctx.action2d("unknown"),
            Vec2::ZERO,
            "unknown actions read as zero"
        );
        assert!(!ctx.action_pressed("unknown"));

        // Deferred commands land at the next flush, not before.
        let deferred = {
            let mut commands = ctx.commands();
            commands.spawn(Hp(99)).id()
        };
        ctx.world().flush();
        assert_eq!(ctx.world().get::<Hp>(deferred), Some(&Hp(99)));

        // No physics in this world.
        assert!(ctx.physics().is_none());
    }

    /// Spec 14 B2: `rand_f32` / `rand_range` come from the seeded `ScriptRng`, so two fresh
    /// worlds draw the same sequence, and `rand_range` stays inside its range.
    #[test]
    fn script_ctx_random_numbers_are_seeded_and_in_range() {
        fn draws() -> Vec<f32> {
            let mut world = World::new();
            let e = world.spawn_empty().id();
            let mut ctx = ScriptCtx::new(&mut world, e);
            let mut out = Vec::new();
            for _ in 0..16 {
                let unit = ctx.rand_f32();
                assert!((0.0..1.0).contains(&unit), "rand_f32 in [0, 1): {unit}");
                out.push(unit);
                let ranged: f32 = ctx.rand_range(-2.0..3.0);
                assert!((-2.0..3.0).contains(&ranged), "rand_range in range: {ranged}");
                out.push(ranged);
            }
            out
        }
        let first = draws();
        assert_eq!(first, draws(), "same seed, same sequence");
        assert!(
            first.windows(2).any(|w| w[0] != w[1]),
            "not a constant: {first:?}"
        );
    }

    /// Spec 14 B6: a message sent from `on_message` (or `on_event`, or `late_update`) waits
    /// until the next `Update`; `ctx.broadcast` reaches every script entity.
    #[test]
    fn messages_sent_while_delivering_wait_for_the_next_update() {
        #[derive(Reflect, Default, Clone)]
        struct Hop(u32);

        #[derive(Reflect, Default, Clone)]
        struct Relay {
            name: String,
            sent: bool,
        }

        impl Script for Relay {
            fn on_message(&mut self, ctx: &mut ScriptCtx, message: &dyn Reflect) {
                if let Some(Hop(n)) = message.as_partial_reflect().try_downcast_ref::<Hop>() {
                    Log::push(ctx.world(), format!("{}:hop{n}:", self.name));
                    if *n == 0 && self.name == "a" {
                        ctx.broadcast(Hop(1));
                    }
                }
            }
            fn late_update(&mut self, ctx: &mut ScriptCtx) {
                if self.name == "a" && !self.sent {
                    self.sent = true;
                    ctx.broadcast(Hop(0));
                }
            }
        }

        let mut app = make_app();
        for name in ["a", "b"] {
            app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Relay".into(),
                order: 0,
                instance: Some(Box::new(Relay {
                    name: name.into(),
                    sent: false,
                })),
                started: false,
                enabled: true,
            }]));
        }
        app.update(); // late_update broadcasts Hop(0)
        assert!(
            app.world().resource::<Log>().0.is_empty(),
            "a late_update message is not delivered in its own frame: {:?}",
            app.world().resource::<Log>().0
        );
        app.update(); // Hop(0) delivered; `a` re-broadcasts Hop(1) from on_message
        let log = app.world().resource::<Log>().0.clone();
        assert_eq!(
            log.iter().filter(|e| e.contains(":hop0:")).count(),
            2,
            "broadcast reaches both scripts: {log:?}"
        );
        assert!(
            !log.iter().any(|e| e.contains(":hop1:")),
            "a message sent from on_message waits for the next Update: {log:?}"
        );
        app.update();
        let log = app.world().resource::<Log>().0.clone();
        assert_eq!(
            log.iter().filter(|e| e.contains(":hop1:")).count(),
            2,
            "delivered on the following Update: {log:?}"
        );
    }

    /// `ScriptsSet::Late` holds the `late_update` pass, so a game system ordered against the set
    /// is ordered against the scripts (the set used to be declared but empty, which gave such a
    /// system no edge at all).
    #[test]
    fn systems_ordered_against_the_late_set_see_late_update() {
        #[derive(Resource, Default)]
        struct Seen {
            before: Vec<bool>,
            after: Vec<bool>,
        }
        #[derive(Resource, Default)]
        struct LateRan(bool);

        #[derive(Reflect, Default, Clone)]
        struct LateWriter;
        impl Script for LateWriter {
            fn late_update(&mut self, ctx: &mut ScriptCtx) {
                ctx.world().resource_mut::<LateRan>().0 = true;
            }
        }

        let mut app = make_app();
        app.init_resource::<Seen>().init_resource::<LateRan>();
        // Added in the same set as the scripts, so only the `ScriptsSet::Late` edge orders them.
        app.add_systems(
            PostUpdate,
            (|ran: Res<LateRan>, mut seen: ResMut<Seen>| seen.after.push(ran.0))
                .in_set(PostUpdateSet::LateScripts)
                .after(ScriptsSet::Late),
        );
        app.add_systems(
            PostUpdate,
            (|ran: Res<LateRan>, mut seen: ResMut<Seen>| seen.before.push(ran.0))
                .in_set(PostUpdateSet::LateScripts)
                .before(ScriptsSet::Late),
        );
        app.add_systems(First, |mut ran: ResMut<LateRan>| ran.0 = false);
        app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
            type_path: "test::LateWriter".into(),
            order: 0,
            instance: Some(Box::new(LateWriter)),
            started: false,
            enabled: true,
        }]));
        for _ in 0..3 {
            app.update();
        }
        let seen = app.world().resource::<Seen>();
        assert_eq!(
            seen.after,
            vec![true; 3],
            "after(ScriptsSet::Late) sees late_update"
        );
        assert_eq!(seen.before, vec![false; 3], "before(ScriptsSet::Late) runs first");
    }

    /// Spec 14 B2: a system can query `Scripts` and downcast an instance to its concrete type.
    #[test]
    fn systems_query_scripts_and_downcast() {
        #[derive(Reflect, Default, Clone)]
        struct Counter {
            count: u32,
        }
        impl Script for Counter {
            fn update(&mut self, _ctx: &mut ScriptCtx) {
                self.count += 1;
            }
        }
        #[derive(Resource, Default)]
        struct Seen(Vec<u32>);

        let mut app = make_app();
        app.init_resource::<Seen>();
        app.add_systems(PostUpdate, |scripts: Query<&Scripts>, mut seen: ResMut<Seen>| {
            for slots in &scripts {
                for slot in slots.0.iter() {
                    let any = slot.instance.as_deref().map(|s| s as &dyn std::any::Any);
                    if let Some(counter) = any.and_then(|a| a.downcast_ref::<Counter>()) {
                        seen.0.push(counter.count);
                    }
                }
            }
        });
        app.world_mut().spawn(Scripts::from_iter([ScriptSlot {
            type_path: "test::Counter".into(),
            order: 0,
            instance: Some(Box::new(Counter::default())),
            started: false,
            enabled: true,
        }]));
        app.update();
        app.update();
        assert_eq!(app.world().resource::<Seen>().0, vec![1, 2]);
    }

    /// Spec 14 B2: `CollisionEnded` and `TriggerExit` reach scripts like their start events.
    #[test]
    fn physics_end_events_reach_scripts() {
        #[derive(Reflect, Default, Clone)]
        struct Ender;
        impl Script for Ender {
            fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
                match event {
                    ScriptEvent::CollisionEnded { entity, .. } if *entity == ctx.entity() => {
                        Log::push(ctx.world(), "collision_ended");
                    }
                    ScriptEvent::TriggerExit { entity, .. } if *entity == ctx.entity() => {
                        Log::push(ctx.world(), "trigger_exit");
                    }
                    _ => {}
                }
            }
        }
        let mut app = make_app();
        app.add_message::<regia_physics::CollisionEnded>();
        app.add_message::<regia_physics::TriggerExit>();
        let me = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Ender".into(),
                order: 0,
                instance: Some(Box::new(Ender)),
                started: false,
                enabled: true,
            }]))
            .id();
        let other = app.world_mut().spawn_empty().id();
        app.world_mut().write_message(regia_physics::CollisionEnded {
            entity_a: other,
            entity_b: me,
        });
        app.world_mut()
            .write_message(regia_physics::TriggerExit { trigger: me, other });
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(log.0, vec!["collision_ended", "trigger_exit"]);
    }

    #[test]
    fn derived_script_macro_constructs_and_orders() {
        // Exercises the real `#[derive(Script)]` from regia_macros end to end. The derive
        // emits `regia_framework::…` paths; alias this crate to that name for the expansion.
        use crate as regia_framework;

        #[derive(regia_macros::Script, Reflect, Default, Clone)]
        #[script(order = -42)]
        #[allow(dead_code)]
        struct Derived {
            field: f32,
        }

        impl Script for Derived {
            fn update(&mut self, ctx: &mut ScriptCtx) {
                Log::push(ctx.world(), "derived:update");
            }
        }

        assert_eq!(<Derived as ReflectScript>::order(), -42);
        assert_eq!(
            <Derived as ReflectScript>::script_type_path(),
            "regia_framework::script::tests::derived_script_macro_constructs_and_orders::Derived"
        );

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ScriptPlugin))
            .register_script::<Derived>()
            .init_resource::<Log>();
        app.world_mut().spawn(Scripts::with::<Derived>());
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        // `Derived` overrides only `update`; the derive supplies construct/order, not methods.
        assert_eq!(log.0, vec!["derived:update".to_string()]);
    }

    /// `register_script` records the app's current owner, and `remove_owner` removes exactly
    /// that owner's entries (spec 20 § 3 step 6: a game library's script registrations go with it).
    #[test]
    fn script_registrations_record_their_owner_and_go_with_it() {
        use crate as regia_framework;

        #[derive(regia_macros::Script, Reflect, Default, Clone)]
        struct GameOwned {
            hp: u32,
        }
        impl Script for GameOwned {}

        #[derive(regia_macros::Script, Reflect, Default, Clone)]
        struct EngineOwned;
        impl Script for EngineOwned {}

        let game = TypeOwner::Game(3);
        let mut app = App::new();
        app.register_script::<EngineOwned>();
        app.with_owner(game, |app| {
            app.register_script::<GameOwned>();
        });
        let mut registry = app.world().resource::<ScriptRegistry>().clone();
        let owned = <GameOwned as ReflectScript>::script_type_path();
        let engine = <EngineOwned as ReflectScript>::script_type_path();
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.owner_of(owned), Some(game));
        assert_eq!(registry.owner_of(engine), Some(TypeOwner::Engine));
        assert_eq!(registry.type_paths_of(game), vec![owned]);
        assert_eq!(registry.remove_owner(game), 1);
        assert_eq!(registry.remove_owner(game), 0, "nothing left to remove");
        assert!(registry.get(owned).is_none());
        assert!(registry.get(engine).is_some());
        assert_eq!(registry.len(), 1);
    }

    /// The derived reflect accessor answers the instance's fields for its own type only: an
    /// instance of another type (another library build of the same path) is refused.
    #[test]
    fn derived_scripts_expose_their_fields_only_to_their_own_type() {
        use crate as regia_framework;
        use regia_ecs::reflect::{ReflectMut, ReflectRef};

        #[derive(regia_macros::Script, Reflect, Default, Clone)]
        struct Health {
            hp: u32,
        }
        impl Script for Health {}

        #[derive(regia_macros::Script, Reflect, Default, Clone)]
        struct Other {
            hp: u32,
        }
        impl Script for Other {}

        let mut registry = ScriptRegistry::default();
        registry.register::<Health>();
        let desc = *registry
            .get(<Health as ReflectScript>::script_type_path())
            .expect("registered");
        let mut health = (desc.factory)();
        {
            let fields = (desc.reflect_mut)(health.as_mut()).expect("own type");
            let ReflectMut::Struct(fields) = fields.reflect_mut() else {
                panic!("a struct script reflects as a struct");
            };
            fields
                .field_mut("hp")
                .expect("hp")
                .try_apply(&7u32)
                .expect("apply");
        }
        let any: &dyn std::any::Any = health.as_ref();
        assert_eq!(any.downcast_ref::<Health>().map(|h| h.hp), Some(7));
        let read = (desc.reflect)(health.as_ref()).expect("own type");
        let ReflectRef::Struct(read) = read.reflect_ref() else {
            panic!("struct");
        };
        assert_eq!(
            read.field("hp").and_then(|f| f.try_downcast_ref::<u32>()),
            Some(&7)
        );

        let mut other: Box<dyn Script> = Box::new(Other { hp: 1 });
        assert!((desc.reflect)(other.as_ref()).is_none());
        assert!((desc.reflect_mut)(other.as_mut()).is_none());
    }

    /// A hand-written `ReflectScript` without the accessors keeps compiling; it answers `None`.
    #[test]
    fn a_hand_written_reflect_script_has_no_reflected_view_by_default() {
        #[derive(Default)]
        struct Manual;
        impl Script for Manual {}
        impl ReflectScript for Manual {
            fn construct() -> Box<dyn Script> {
                Box::new(Self)
            }
            fn order() -> i32 {
                0
            }
            fn script_type_path() -> &'static str {
                "tests::Manual"
            }
        }
        let instance = Manual::construct();
        assert!(<Manual as ReflectScript>::reflect_script(instance.as_ref()).is_none());
    }

    /// The physics bridge: a `TriggerEnter` message reaches only the trigger entity's
    /// scripts, with the trigger as `entity` and the entering body as `other`.
    #[test]
    fn physics_trigger_events_reach_only_the_trigger_entity() {
        #[derive(Reflect, Default, Clone)]
        struct Collector {
            name: &'static str,
        }

        impl Script for Collector {
            fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
                let ScriptEvent::TriggerEnter { entity, other } = event else {
                    return;
                };
                let is_trigger = *entity == ctx.entity();
                Log::push(
                    ctx.world(),
                    format!("{}:{is_trigger}:{}", self.name, other.index()),
                );
            }
        }

        let mut app = make_app();
        // Register the message the way `PhysicsPlugin` does (schedules the buffer swap).
        app.add_message::<regia_physics::TriggerEnter>();
        let sensor = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Collector".into(),
                order: 0,
                instance: Some(Box::new(Collector { name: "sensor" })),
                started: false,
                enabled: true,
            }]))
            .id();
        let bystander = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Collector".into(),
                order: 0,
                instance: Some(Box::new(Collector { name: "bystander" })),
                started: false,
                enabled: true,
            }]))
            .id();
        let walker = app.world_mut().spawn_empty().id();

        // The physics plugin is not installed; write the message by hand the way
        // `flush_physics_events` would.
        app.world_mut().write_message(regia_physics::TriggerEnter {
            trigger: sensor,
            other: walker,
        });
        app.update();

        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(
            log.0,
            vec![format!("sensor:true:{}", walker.index())],
            "only the trigger's script sees the event; bystander hears nothing: {:?}",
            log.0
        );
        let _ = bystander;
    }

    /// The physics bridge: a collision reaches both participants, each seeing itself as
    /// `entity`.
    #[test]
    fn physics_collision_events_reach_both_participants() {
        #[derive(Reflect, Default, Clone)]
        struct Bumper;

        impl Script for Bumper {
            fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
                if let ScriptEvent::CollisionStarted { entity, other } = event {
                    Log::push(ctx.world(), format!("{}:{}", entity.index(), other.index()));
                }
            }
        }

        let mut app = make_app();
        // Register the message the way `PhysicsPlugin` does (schedules the buffer swap).
        app.add_message::<regia_physics::CollisionStarted>();
        let a = app
            .world_mut()
            .spawn(Scripts::from_iter([ScriptSlot {
                type_path: "test::Bumper".into(),
                order: 0,
                instance: Some(Box::new(Bumper)),
                started: false,
                enabled: true,
            }]))
            .id();
        let b = app.world_mut().spawn_empty().id();

        app.world_mut().write_message(regia_physics::CollisionStarted {
            entity_a: a,
            entity_b: b,
        });
        app.update();

        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        assert_eq!(log.0, vec![format!("{}:{}", a.index(), b.index())]);
    }

    /// Scripts run in the order their `Scripts` arrived, not in entity-id order: parallel
    /// `Commands::spawn` calls reserve ids in thread-timing order (ADR-1347).
    #[test]
    fn scripts_run_in_arrival_order_not_entity_id_order() {
        let mut app = make_app();
        let low = app.world_mut().spawn_empty().id();
        let high = app.world_mut().spawn_empty().id();
        let scripts = |name: &str| {
            Scripts::from_iter([ScriptSlot {
                type_path: format!("test::{name}"),
                order: 0,
                instance: Some(Box::new(Recorder { name: name.into() })),
                started: false,
                enabled: true,
            }])
        };
        // The higher id gets its scripts first.
        app.world_mut().entity_mut(high).insert(scripts("first"));
        app.world_mut().entity_mut(low).insert(scripts("second"));
        app.update();
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default();
        let updates: Vec<&str> = log
            .0
            .iter()
            .map(String::as_str)
            .filter(|entry| entry.ends_with(":update"))
            .collect();
        assert_eq!(updates, vec!["first:update", "second:update"]);
    }

    /// Spec 15 § 3 (ADR-7233): a `ContactForce` message reaches the scripts of both participants
    /// as [`ScriptEvent::ContactForce`], each seeing itself as `entity`, with the force.
    #[test]
    fn contact_force_events_reach_both_participants_with_the_force() {
        #[derive(Reflect, Default, Clone)]
        struct Crate;

        impl Script for Crate {
            fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
                if let ScriptEvent::ContactForce {
                    entity,
                    other,
                    total_force,
                } = event
                {
                    Log::push(
                        ctx.world(),
                        format!("{}:{}:{total_force}", entity.index(), other.index()),
                    );
                }
            }
        }

        let slot = || {
            Scripts::from_iter([ScriptSlot {
                type_path: "test::Crate".into(),
                order: 0,
                instance: Some(Box::new(Crate)),
                started: false,
                enabled: true,
            }])
        };
        let mut app = make_app();
        app.add_message::<regia_physics::ContactForce>();
        let a = app.world_mut().spawn(slot()).id();
        let b = app.world_mut().spawn(slot()).id();
        let bystander = app.world_mut().spawn(slot()).id();
        app.world_mut().write_message(regia_physics::ContactForce {
            entity_a: a,
            entity_b: b,
            total_force: 250.0,
        });
        app.update();

        let mut log = app.world_mut().remove_resource::<Log>().unwrap_or_default().0;
        log.sort();
        let mut expected = vec![
            format!("{}:{}:250", a.index(), b.index()),
            format!("{}:{}:250", b.index(), a.index()),
        ];
        expected.sort();
        assert_eq!(log, expected, "both participants, nobody else ({bystander:?})");
    }

    /// End to end with the physics step: a box landing hard on a plate raises `ContactForce`
    /// above its threshold, and the box's script receives it naming the plate.
    #[test]
    fn a_hard_landing_reaches_the_script_as_a_contact_force_event() {
        use regia_physics::{ActiveEvents, Collider, ColliderShape, PhysicsPlugin, RigidBody};
        use regia_scene::Transform;

        #[derive(Reflect, Default, Clone)]
        struct Landing;

        impl Script for Landing {
            fn on_event(&mut self, ctx: &mut ScriptCtx, event: &ScriptEvent) {
                if let ScriptEvent::ContactForce {
                    entity,
                    other,
                    total_force,
                } = *event
                    && entity == ctx.entity()
                {
                    Log::push(ctx.world(), format!("{}:{}", other.index(), total_force > 1.0));
                }
            }
        }

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ScriptPlugin, PhysicsPlugin))
            .init_resource::<Log>()
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )));
        let mut fixed = regia_app::FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed);
        let events = |shape: ColliderShape| Collider {
            shape,
            active_events: ActiveEvents::ALL,
            contact_force_threshold: 1.0,
            ..Collider::default()
        };
        let plate = app
            .world_mut()
            .spawn((
                Transform::from_translation(regia_core::math::Vec3::new(0.0, -0.5, 0.0)),
                RigidBody::static_body(),
                events(ColliderShape::Box {
                    half_extents: regia_core::math::Vec3::new(5.0, 0.5, 5.0),
                }),
            ))
            .id();
        app.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(0.0, 3.0, 0.0)),
            RigidBody::dynamic(),
            events(ColliderShape::Box {
                half_extents: regia_core::math::Vec3::splat(0.5),
            }),
            Scripts::from_iter([ScriptSlot {
                type_path: "test::Landing".into(),
                order: 0,
                instance: Some(Box::new(Landing)),
                started: false,
                enabled: true,
            }]),
        ));
        for _ in 0..120 {
            app.update();
        }
        let log = app.world_mut().remove_resource::<Log>().unwrap_or_default().0;
        assert!(!log.is_empty(), "the landing reached the script");
        let expected = format!("{}:true", plate.index());
        assert!(
            log.iter().all(|entry| *entry == expected),
            "every event names the plate with a force over the threshold: {log:?}"
        );
    }

    /// `ScriptCtx::teleport` moves a body (Transform + the `Teleport` marker, velocity cleared on
    /// request) and refuses an entity without one.
    #[test]
    fn script_ctx_teleport_marks_a_body_and_refuses_others() {
        use regia_physics::{RigidBody, Teleport, TeleportVelocity, Velocity};
        let mut world = World::new();
        let body = world
            .spawn((
                Transform::IDENTITY,
                RigidBody::dynamic(),
                Velocity {
                    linear: regia_core::math::Vec3::X,
                    angular: regia_core::math::Vec3::ZERO,
                },
            ))
            .id();
        let plain = world.spawn(Transform::IDENTITY).id();
        let target = Transform::from_translation(regia_core::math::Vec3::new(1.0, 2.0, 3.0));
        assert!(ScriptCtx::new(&mut world, body).teleport(target, TeleportVelocity::Clear));
        assert_eq!(world.get::<Transform>(body), Some(&target));
        assert!(world.get::<Teleport>(body).is_some());
        assert_eq!(
            world.get::<Velocity>(body).map(|v| v.linear),
            Some(regia_core::math::Vec3::ZERO)
        );
        assert!(!ScriptCtx::new(&mut world, plain).teleport(target, TeleportVelocity::Keep));
        assert_eq!(world.get::<Transform>(plain), Some(&Transform::IDENTITY));
    }
}
