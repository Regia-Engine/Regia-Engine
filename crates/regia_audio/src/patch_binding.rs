// SPDX-License-Identifier: MPL-2.0
//! Patch parameters bound to reflected fields (`.audiopatch.ron` v2, ADR-7226,
//! `docs/specs/16-audio.md` § 3a): "parameters set from Rust or bound to reflected fields".
//!
//! An entity carrying [`AudioPatchSource`] names a spawned patch. Every frame, before the audio
//! tick, [`patch_binding_sync`] reads each bound parameter's `component.field` on that entity
//! through the type registry and sets the parameter's target to `value * scale + offset`. The
//! parameter then clamps and slews as if the game had called `Audio::patch_set_param`. A missing
//! component, an unknown name or an unreadable leaf leaves the parameter where it is and warns
//! once per entity and binding.

use std::any::TypeId;
use std::collections::{HashMap, HashSet};

use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};

use crate::audio::Audio;

/// Connects an entity to a spawned patch: the patch's bound parameters read this entity's
/// reflected components.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioPatchSource {
    /// The id [`Audio::spawn_patch`] returned.
    pub patch: u64,
}

/// One bound value read this frame.
struct BoundWrite {
    patch: u64,
    index: usize,
    value: f32,
}

/// The binding system's caches and counters.
#[derive(Resource, Default)]
pub struct PatchBindingState {
    registry_count: usize,
    by_short: HashMap<String, TypeId>,
    reflect: HashMap<TypeId, ReflectComponent>,
    sources: Vec<(Entity, u64)>,
    writes: Vec<BoundWrite>,
    warned: HashSet<(Entity, u64, usize)>,
    reads: u64,
    failures: u64,
}

impl PatchBindingState {
    /// Bound values read and applied since start-up.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    /// Bound reads that failed (missing component, unknown name or path, unreadable leaf).
    pub fn failures(&self) -> u64 {
        self.failures
    }
}

/// Reads a numeric or boolean reflected leaf as `f32`.
pub fn reflected_f32(leaf: &dyn regia_ecs::reflect::PartialReflect) -> Option<f32> {
    macro_rules! try_as {
        ($($ty:ty),*) => {
            $(if let Some(v) = leaf.try_downcast_ref::<$ty>() {
                return Some(*v as f32);
            })*
        };
    }
    try_as!(f32, f64, i32, u32, i64, u64, u8, u16, i16);
    leaf.try_downcast_ref::<bool>().map(|b| f32::from(u8::from(*b)))
}

/// Rebuilds the short-name map when the registry's registration count changed. Two components
/// sharing a short name resolve to the smallest full type path (as property animation does).
fn refresh_names(state: &mut PatchBindingState, registry: &regia_ecs::reflect::TypeRegistry) {
    let count = registry.iter().size_hint().0;
    if state.registry_count == count && !state.by_short.is_empty() {
        return;
    }
    let mut names: Vec<(String, String, TypeId)> = registry
        .iter()
        .filter_map(|registration| {
            registration.data::<ReflectComponent>()?;
            let path = registration.type_info().type_path();
            let short = path.rsplit("::").next()?;
            Some((short.to_string(), path.to_string(), registration.type_id()))
        })
        .collect();
    names.sort();
    state.by_short.clear();
    for (short, _, id) in names {
        state.by_short.entry(short).or_insert(id);
    }
    state.registry_count = count;
}

/// `PostUpdate` system (before `AudioSync`): sets every bound patch parameter from its reflected
/// field. Inert without an [`Audio`] resource or an [`AppTypeRegistry`].
#[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
pub fn patch_binding_sync(world: &mut World) {
    if !world.contains_resource::<Audio>() {
        return;
    }
    let Some(registry) = world.get_resource::<AppTypeRegistry>().cloned() else {
        return;
    };
    if !world.contains_resource::<PatchBindingState>() {
        world.init_resource::<PatchBindingState>();
    }
    world.resource_scope::<PatchBindingState, _>(|world, mut state| {
        let state = &mut *state;
        state.sources.clear();
        let mut query = world.query::<(Entity, &AudioPatchSource)>();
        state
            .sources
            .extend(query.iter(world).map(|(entity, source)| (entity, source.patch)));
        if state.sources.is_empty() {
            return;
        }
        let registry = registry.read();
        refresh_names(state, &registry);
        state.writes.clear();
        let audio = world.resource::<Audio>();
        for &(entity, patch) in &state.sources {
            let Some(def) = audio.patch_def(patch) else {
                continue;
            };
            for (index, param) in def.parameters.iter().enumerate() {
                let Some(binding) = &param.binding else {
                    continue;
                };
                let type_id = state.by_short.get(&binding.component).copied();
                let reflect = type_id.and_then(|id| {
                    if let Some(found) = state.reflect.get(&id) {
                        return Some(found.clone());
                    }
                    let found = registry.get(id)?.data::<ReflectComponent>()?.clone();
                    state.reflect.insert(id, found.clone());
                    Some(found)
                });
                let read = reflect.and_then(|reflect| {
                    let entity_ref = world.get_entity(entity).ok()?;
                    let component = reflect.reflect(entity_ref)?;
                    let leaf = component.reflect_path(binding.field.as_str()).ok()?;
                    reflected_f32(leaf)
                });
                match read {
                    Some(raw) => {
                        let value = raw * binding.scale + binding.offset;
                        state.reads += 1;
                        state.writes.push(BoundWrite { patch, index, value });
                    }
                    None => {
                        state.failures += 1;
                        if state.warned.insert((entity, patch, index)) {
                            tracing::warn!(
                                target: "regia_audio",
                                "patch {patch} parameter `{}`: cannot read {}.{} on {entity:?} \
                                 (missing component, unregistered type, bad path or non-numeric leaf)",
                                param.name, binding.component, binding.field
                            );
                        }
                    }
                }
            }
        }
        let mut audio = world.resource_mut::<Audio>();
        for write in &state.writes {
            audio.patch_set_param_at(write.patch, write.index, write.value);
        }
    });
}
