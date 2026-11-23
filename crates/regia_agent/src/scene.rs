// SPDX-License-Identifier: MPL-2.0
//! Scene surface (specs/23 § 3.2): query with filters, inspect, and edit entities through
//! reflection — spawn/despawn/duplicate/reparent, component add/remove, property set, and
//! `apply_edits` transactions. Editor-only tools (select, frame, undo/redo, save_level)
//! deliberately live with the editor.

use regia_app::prelude::*;
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{GetPath, PartialReflect, ReflectComponent, TypeRegistration, TypeRegistry};
use regia_ecs::serialize::ReflectCodec;
use serde::de::DeserializeSeed;
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

/// Looks a reflected component registration up by short or full type name.
pub(crate) fn registration<'r>(
    registry: &'r TypeRegistry,
    name: &str,
) -> Result<&'r TypeRegistration, RpcError> {
    registry
        .get_with_short_type_path(name)
        .or_else(|| regia_core::brand::lookup_type_path(name, |p| registry.get_with_type_path(p)))
        .ok_or_else(|| {
            RpcError::not_found(
                format!("no reflected type {name}"),
                "call types.list for the registered types, or scene.get_entity for what an entity has",
            )
        })
}

/// The reflected-component data of a registration (the protocol can only edit reflected
/// components).
/// A resolved component type: its registration and reflected-component data.
pub(crate) struct ResolvedComponent<'r> {
    /// The type registration (docs, schema, serde data).
    pub registration: &'r TypeRegistration,
    /// The reflected-component data (`insert`, `remove`, `reflect`).
    pub data: &'r ReflectComponent,
}

/// Resolves a component by name and returns its reflected data.
pub(crate) fn resolve_component<'r>(
    registry: &'r TypeRegistry,
    name: &str,
) -> Result<ResolvedComponent<'r>, RpcError> {
    let registration = registration(registry, name)?;
    let data = registration.data::<ReflectComponent>().ok_or_else(|| {
        RpcError::not_found(
            format!("{name} is not a reflected component"),
            "only components registered with reflection can be edited",
        )
    })?;
    Ok(ResolvedComponent { registration, data })
}

/// Serialises a reflected value to JSON (fallible; callers decide how to report errors).
pub(crate) fn serialize_reflect(
    value: &dyn PartialReflect,
    registry: &TypeRegistry,
) -> Result<Value, String> {
    serde_json::to_value(TypedReflectSerializer::new(value, registry)).map_err(|e| e.to_string())
}

/// Deserialises a component value from JSON using the registration's reflected serde data.
fn deserialize_component(
    resolved: &ResolvedComponent<'_>,
    value: &Value,
    registry: &TypeRegistry,
) -> Result<Box<dyn PartialReflect>, RpcError> {
    TypedReflectDeserializer::new(resolved.registration, registry)
        .deserialize(value)
        .map(|v: Box<dyn PartialReflect>| v)
        .map_err(|e| {
            RpcError::invalid_params(format!(
                "value does not fit {}: {e}",
                resolved.registration.type_info().type_path()
            ))
        })
}

/// Resolves the `entity` parameter.
pub(crate) fn entity_from(params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (the id from scene.query_entities) is required"))?;
    Entity::try_from_bits(bits).ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

/// Errors when the entity does not exist, with the query hint.
/// Refuses an edit of a feed-owned entity (specs/60 § 2.4 rule 1): `F0002`, reason `feed_owned`.
/// The DCC stream owns it until a bake; selecting, framing and adding children stay allowed.
///
/// # Errors
///
/// The refusal, when `entity` carries `FeedOwned`.
pub fn require_not_feed_owned(world: &World, entity: Entity) -> Result<(), RpcError> {
    let Some(owned) = world.get::<regia_scene::prelude::FeedOwned>(entity) else {
        return Ok(());
    };
    let mut error = RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "F0002",
        format!(
            "entity {} is owned by the DCC feed item {:?}",
            entity.to_bits(),
            owned.key
        ),
        "bake it with dcc_feed.bake, or change it in the content tool",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!("feed_owned"));
    }
    Err(error)
}

fn require_entity(world: &World, entity: Entity) -> Result<(), RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    // Resources are entities too (bevy_ecs 0.20). Despawning `Time` or the audit log from the
    // protocol would abort the next frame, so they are not scene entities here (ADR-1034).
    if world
        .get::<regia_ecs::ecs::resource::IsResource>(entity)
        .is_some()
    {
        return Err(RpcError::not_found(
            format!(
                "entity {} holds an engine resource, not scene content",
                entity.to_bits()
            ),
            "use scene.query_entities for scene entities; resources.list for resources",
        ));
    }
    Ok(())
}

/// `*` glob match (case-sensitive, `*` = any run of characters, everything else literal).
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else {
                return false;
            };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(pos) = rest.find(part) else {
                return false;
            };
            rest = &rest[pos + part.len()..];
        }
    }
    true
}

/// Entity name, if any: the ECS `Name` component, else the scene's
/// `regia_scene::components::Name` (what scenes, prefabs and samples spawn). Before ADR-2134 only
/// the first was read, so `{"name": ..}` selectors missed scene-named entities.
pub(crate) fn entity_name(world: &World, entity: Entity) -> Option<String> {
    world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_string())
        .or_else(|| {
            world
                .get::<regia_scene::components::Name>(entity)
                .map(|n| n.as_str().to_string())
        })
}

/// Component type names on an entity: `(short, full, type_id)`.
pub(crate) fn components_of(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
) -> Vec<(String, String, Option<std::any::TypeId>)> {
    let Ok(infos) = world.inspect_entity(entity) else {
        return Vec::new();
    };
    infos
        .map(|(_, info)| {
            let full = info.name().to_string();
            let type_id = info.type_id();
            let short = type_id
                .and_then(|t| registry.get(t))
                .map(|r| r.type_info().type_path_table().short_path().to_string())
                .unwrap_or_else(|| full.rsplit("::").next().unwrap_or(&full).to_string());
            (short, full, type_id)
        })
        .collect()
}

/// Reads an entity's component through reflection and serialises it (None when the entity
/// lacks the component or it is not reflected).
pub(crate) fn reflected_value(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
    component: &str,
) -> Result<Option<Value>, RpcError> {
    let resolved = resolve_component(registry, component)?;
    let Ok(entity_ref) = world.get_entity(entity) else {
        return Ok(None);
    };
    let Some(value) = resolved.data.reflect(entity_ref) else {
        return Ok(None);
    };
    serialize_reflect(value.as_partial_reflect(), registry)
        .map(Some)
        .map_err(|e| RpcError::internal(format!("cannot serialize {component}: {e}")))
}

/// Extracts a 3-component vector (`[x, y, z]`) from a serialised value: glam serialises as
/// `{x, y, z}` objects, tuples as arrays.
fn vec3_from_value(value: &Value) -> Option<[f64; 3]> {
    match value {
        Value::Array(items) if items.len() == 3 => {
            Some([items[0].as_f64()?, items[1].as_f64()?, items[2].as_f64()?])
        }
        Value::Object(map) => {
            let g = |k: &str| map.get(k).and_then(Value::as_f64);
            Some([g("x")?, g("y")?, g("z")?])
        }
        _ => None,
    }
}

/// The world-space translation of an entity's `Transform` (f64 for wide dynamic range), or
/// `None` when the entity has no reflected `Transform`.
pub(crate) fn translation_of(world: &World, registry: &TypeRegistry, entity: Entity) -> Option<[f64; 3]> {
    let value = reflected_value(world, registry, entity, "Transform").ok()??;
    let translation = value.get("translation")?;
    vec3_from_value(translation)
}

/// Three coordinates from a JSON array; a missing or `null` element is 0, a non-number is refused.
fn point3(items: &[Value], param: &str) -> Result<[f64; 3], RpcError> {
    let mut out = [0.0; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = match items.get(i) {
            None | Some(Value::Null) => 0.0,
            Some(v) => v
                .as_f64()
                .ok_or_else(|| RpcError::wrong_type(&format!("{param}[{i}]"), "a number"))?,
        };
    }
    Ok(out)
}

/// `scene.query_entities`: filter by name glob, component, and position.
pub(crate) fn query_entities(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name_glob = crate::params::opt_str(params, "name")?;
    let component = crate::params::opt_str(params, "component")?;
    // A kind name, `none` or `unclassified` (spec 101 § 7).
    let kind_filter = crate::params::opt_str(params, "kind")?;
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(200) as usize;
    // A present `near` / `box` of the wrong shape is refused (ADR-2622). A `near` without both
    // `radius` and `point`, or a `box` without both corners, filters nothing, as before.
    let near = match crate::params::opt_object(params, "near")? {
        None => None,
        Some(n) => match (
            crate::params::opt_f64(n, "radius")?,
            crate::params::opt_array(n, "point")?,
        ) {
            (Some(radius), Some(point)) => Some((point3(point, "near.point")?, radius)),
            _ => None,
        },
    };
    let within_box = match crate::params::opt_object(params, "box")? {
        None => None,
        Some(b) => match (
            crate::params::opt_array(b, "min")?,
            crate::params::opt_array(b, "max")?,
        ) {
            (Some(min), Some(max)) => Some((point3(min, "box.min")?, point3(max, "box.max")?)),
            _ => None,
        },
    };
    if (near.is_some() || within_box.is_some()) && params.get("component").is_none() {
        // Spatial filters read `Transform` only; say so instead of failing silently.
        tracing::debug!(target: "regia_agent", "spatial query without a component filter");
    }

    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut q = world.query::<Entity>();
    let mut entities: Vec<Entity> = q.iter(world).collect();
    // Archetype order shifts when components are added; sort so `limit` keeps a stable subset.
    entities.sort_by_key(|e| e.to_bits());
    let mut out = Vec::new();
    for entity in entities {
        // Checked before the push so `limit: 0` is an empty page.
        if out.len() >= limit {
            break;
        }
        let name = entity_name(world, entity);
        if let Some(glob) = name_glob
            && !name.as_deref().is_some_and(|n| glob_match(glob, n))
        {
            continue;
        }
        let comps = components_of(world, &registry, entity);
        if let Some(c) = component
            && !comps.iter().any(|(short, full, _)| short == c || full == c)
        {
            continue;
        }
        if let Some(kind) = kind_filter
            && !crate::kinds::kind_matches(world, &registry, entity, kind)
        {
            continue;
        }
        let translation = translation_of(world, &registry, entity);
        if let Some((point, radius)) = near {
            let Some(t) = translation else {
                continue;
            };
            let dx = t[0] - point[0];
            let dy = t[1] - point[1];
            let dz = t[2] - point[2];
            if dx * dx + dy * dy + dz * dz > radius * radius {
                continue;
            }
        }
        if let Some((min, max)) = within_box {
            let Some(t) = translation else {
                continue;
            };
            if t.iter().zip(min.iter()).any(|(v, m)| v < m) || t.iter().zip(max.iter()).any(|(v, m)| v > m) {
                continue;
            }
        }
        let kind = regia_scene::kind::kind_of(world, entity).map(|k| k.name);
        let parent = world.get::<ChildOf>(entity).map(|c| c.parent().to_bits());
        out.push(json!({
            "entity": entity.to_bits(),
            "name": name,
            "kind": kind,
            "parent": parent,
            "components": comps.iter().map(|(short, _, _)| short.clone()).collect::<Vec<_>>(),
            "translation": translation.map(|t| json!([t[0], t[1], t[2]])),
        }));
    }
    let count = out.len();
    Ok(json!({ "entities": out, "count": count }))
}

/// Resolves an entity reference: a numeric id, `{"name": glob}` or `{"component": type}` —
/// re-resolved on every evaluation so conditions survive entity churn.
pub(crate) fn resolve_entity(world: &World, reference: &Value) -> Result<Entity, String> {
    match reference {
        Value::Number(_) | Value::String(_) => {
            let bits = reference
                .as_u64()
                .or_else(|| reference.as_str().and_then(|s| s.parse().ok()))
                .ok_or_else(|| "entity reference must be an id".to_string())?;
            let entity = Entity::try_from_bits(bits).ok_or_else(|| format!("{bits} is not an entity id"))?;
            if world.get_entity(entity).is_err() {
                return Err(format!("entity {bits} does not exist"));
            }
            Ok(entity)
        }
        Value::Object(fields) => {
            let registry = world.resource::<AppTypeRegistry>().clone();
            let registry = registry.read();
            // Lowest id wins: archetype order is not stable across component adds.
            let mut candidates: Vec<Entity> = world.iter_entities().map(|e| e.id()).collect();
            candidates.sort_by_key(|e| e.to_bits());
            let name_glob = crate::params::opt_str(fields, "name").map_err(|e| e.message)?;
            let component_ref = crate::params::opt_str(fields, "component").map_err(|e| e.message)?;
            for entity in candidates {
                if let Some(glob) = name_glob
                    && !entity_name(world, entity).is_some_and(|n| glob_match(glob, &n))
                {
                    continue;
                }
                if let Some(component) = component_ref {
                    let comps = components_of(world, &registry, entity);
                    if !comps
                        .iter()
                        .any(|(short, full, _)| short == component || full == component)
                    {
                        continue;
                    }
                }
                return Ok(entity);
            }
            Err(format!(
                "no entity matches {reference} — use scene.query_entities to see what exists"
            ))
        }
        other => Err(format!("bad entity reference: {other}")),
    }
}

/// `scene.get_entity`: the components of one entity with their reflected values.
pub(crate) fn get_entity(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    require_entity(world, entity)?;
    let only: Option<Vec<String>> = crate::params::opt_array(params, "components")?
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut components = Map::new();
    for (short, full, type_id) in components_of(world, &registry, entity) {
        if only
            .as_ref()
            .is_some_and(|o| !o.iter().any(|c| c == &short || c == &full))
        {
            continue;
        }
        let reflect_component = type_id
            .and_then(|t| registry.get(t))
            .and_then(|r| r.data::<ReflectComponent>());
        let value = match reflect_component.and_then(|rc| rc.reflect(world.entity(entity))) {
            Some(value) => match serialize_reflect(value.as_partial_reflect(), &registry) {
                Ok(v) => json!({ "type": full, "value": v }),
                Err(e) => json!({ "type": full, "error": format!("not serializable: {e}") }),
            },
            None => json!({ "type": full, "reflected": false }),
        };
        components.insert(short, value);
    }
    let parent = world.get::<ChildOf>(entity).map(|c| c.parent().to_bits());
    let (kind, role, unclassified) = crate::kinds::entity_kind_json(world, &registry, entity);
    Ok(json!({
        "entity": entity.to_bits(),
        "name": entity_name(world, entity),
        "parent": parent,
        "kind": kind,
        "role": role,
        "unclassified": unclassified,
        "components": Value::Object(components),
    }))
}

/// The optional `dry_run` flag: absent or null is `false`; anything but a JSON boolean is
/// rejected (a string like `"true"` must not silently perform the write).
pub(crate) fn dry_run_flag(params: &Value) -> Result<bool, RpcError> {
    match params.get("dry_run").filter(|v| !v.is_null()) {
        None => Ok(false),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`dry_run` must be a boolean")),
    }
}

/// `scene.set_property`: write a reflected field (or whole component) of one entity.
fn set_property_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` (type name, e.g. \"Transform\") is required"))?;
    let path = crate::params::opt_str(params, "path")?.unwrap_or("");
    let value = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let dry_run = dry_run_flag(params)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let resolved = resolve_component(&registry, component)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let mut entity_mut = world.entity_mut(entity);
    let Some(mut target) = resolved.data.reflect_mut(&mut entity_mut) else {
        return Err(RpcError::not_found(
            format!("entity has no {component}"),
            "add the component first (scene.add_component)",
        ));
    };
    let field: &mut dyn PartialReflect = if path.is_empty() {
        target.as_partial_reflect_mut()
    } else {
        target.reflect_path_mut(path).map_err(|e| {
            RpcError::not_found(
                format!("no field `{path}` in {component}: {e}"),
                "see the value returned by scene.get_entity for field names",
            )
        })?
    };
    let field_type = field
        .get_represented_type_info()
        .map(|info| info.type_id())
        .and_then(|t| registry.get(t))
        .ok_or_else(|| RpcError::internal(format!("field `{path}` has no registered type")))?;
    let new_value = TypedReflectDeserializer::new(field_type, &registry)
        .deserialize(value)
        .map_err(|e| {
            RpcError::invalid_params(format!(
                "value does not fit `{path}` ({}): {e}",
                field_type.type_info().type_path()
            ))
        })?;
    let before = serialize_reflect(field, &registry).unwrap_or(Value::Null);
    if !dry_run {
        field
            .try_apply(new_value.as_ref())
            .map_err(|e| RpcError::invalid_params(format!("cannot apply value: {e}")))?;
    }
    let after = if dry_run {
        serialize_reflect(new_value.as_ref(), &registry)
    } else {
        serialize_reflect(field, &registry)
    };
    Ok(json!({ "before": before, "after": after.unwrap_or(Value::Null), "applied": !dry_run }))
}

/// One component prepared for spawn: its type path, its deserialised value, and the
/// reflected-component data that inserts it.
struct SpawnComponent {
    /// Registered type path, for logging.
    type_path: &'static str,
    /// The deserialised value to insert.
    value: Box<dyn PartialReflect>,
    /// The reflected-component data that performs the insert.
    data: ReflectComponent,
}

/// Builds the list of components to insert on spawn: resolves names and deserialises values.
fn build_components(
    registry: &TypeRegistry,
    components: &Map<String, Value>,
) -> Result<Vec<SpawnComponent>, RpcError> {
    let mut built = Vec::new();
    for (name, value) in components {
        let resolved = resolve_component(registry, name)?;
        let value = deserialize_component(&resolved, value, registry)?;
        built.push(SpawnComponent {
            type_path: resolved.registration.type_info().type_path(),
            value,
            data: resolved.data.clone(),
        });
    }
    Ok(built)
}

/// `scene.spawn`: a new entity from reflected components, a name, and a parent.
fn spawn_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let components = params.get("components").cloned().unwrap_or_else(|| json!({}));
    let Some(map) = components.as_object() else {
        return Err(RpcError::invalid_params(
            "`components` must be an object of { \"Type\": value }",
        ));
    };
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let built = build_components(&registry, map)?;

    // Resolve the parent first: spawning borrows the world mutably for the inserts below.
    let parent = match crate::params::opt_u64(params, "parent")? {
        Some(bits) => {
            let parent = Entity::try_from_bits(bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
            require_entity(world, parent)?;
            Some(parent)
        }
        None => None,
    };

    let name = crate::params::opt_str(params, "name")?
        .filter(|n| !n.is_empty())
        .map(String::from);
    let mut entity = world.spawn_empty();
    if let Some(name) = name {
        entity.insert(Name::new(name));
    }
    for component in built {
        component
            .data
            .insert(&mut entity, component.value.as_ref(), &registry);
        tracing::debug!(
            target: "regia_agent",
            "spawned {} on {:?}",
            component.type_path,
            entity.id()
        );
    }
    if let Some(parent) = parent {
        entity.insert(ChildOf(parent));
    }
    Ok(json!({ "entity": entity.id().to_bits() }))
}

/// `scene.despawn`: remove an entity (and, through the `ChildOf` relationship, its children).
fn despawn_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    // Iterative (`despawn_tree`, ADR-2306): a deep hierarchy cannot overflow the server's stack.
    let despawned = regia_scene::hierarchy::despawn_tree(world, entity)
        .then_some(true)
        .ok_or_else(|| RpcError::internal("despawn failed"))?;
    Ok(json!({ "despawned": despawned }))
}

/// Relationship components are rebuilt by the ECS from their source components; copying them
/// would fight the relationship hooks, so duplicate skips them.
fn is_relationship(short: &str) -> bool {
    matches!(short, "ChildOf" | "Children")
}

/// `scene.duplicate`: copy an entity's reflected components to a new entity.
fn duplicate_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut copies = Vec::new();
    let mut skipped = Vec::new();
    for (short, _, type_id) in components_of(world, &registry, entity) {
        if is_relationship(&short) {
            skipped.push(short);
            continue;
        }
        let Some(data) = type_id
            .and_then(|t| registry.get(t))
            .and_then(|r| r.data::<ReflectComponent>())
        else {
            skipped.push(short);
            continue;
        };
        let Some(value) = data.reflect(world.entity(entity)) else {
            skipped.push(short);
            continue;
        };
        let Ok(cloned) = value.reflect_clone() else {
            skipped.push(short);
            continue;
        };
        copies.push((short, data.clone(), cloned));
    }
    let parent = crate::params::opt_u64(params, "parent")?
        .map(|bits| {
            Entity::try_from_bits(bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        })
        .transpose()?
        .or_else(|| world.get::<ChildOf>(entity).map(|c| c.parent()));
    if let Some(parent) = parent {
        require_entity(world, parent)?;
    }
    let mut copy = world.spawn_empty();
    let copied = copies.len();
    for (_, data, value) in copies {
        data.insert(&mut copy, value.as_partial_reflect(), &registry);
    }
    if let Some(parent) = parent {
        copy.insert(ChildOf(parent));
    }
    Ok(json!({ "entity": copy.id().to_bits(), "copied_components": copied, "skipped": skipped }))
}

/// `scene.reparent`: move an entity under a new parent (or detach it).
fn reparent_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let parent = match params.get("parent") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`parent` must be an entity id or null"))?;
            let parent = Entity::try_from_bits(bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
            require_entity(world, parent)?;
            Some(parent)
        }
    };
    if let Some(parent) = parent
        && would_cycle(world, entity, parent)
    {
        return Err(RpcError::invalid_params(
            "an entity cannot be its own parent or the parent of one of its ancestors",
        ));
    }
    let mut entity_mut = world.entity_mut(entity);
    match parent {
        Some(parent) => {
            entity_mut.insert(ChildOf(parent));
        }
        None => {
            entity_mut.remove::<ChildOf>();
        }
    }
    Ok(json!({ "entity": entity.to_bits(), "parent": parent.map(|p| p.to_bits()) }))
}

/// `scene.add_component`: insert a reflected component (default value when none given).
fn add_component_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` (type name) is required"))?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let resolved = resolve_component(&registry, component)?;
    let value: Box<dyn PartialReflect> = match params.get("value") {
        Some(value) => deserialize_component(&resolved, value, &registry)?,
        None => {
            use regia_ecs::reflect::std_traits::ReflectDefault;
            let data = resolved.registration.data::<ReflectDefault>().ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "{component} has no registered default; pass `value` in the shape \
                     types.describe shows"
                ))
            })?;
            data.default()
        }
    };
    let mut entity_mut = world.entity_mut(entity);
    resolved.data.insert(&mut entity_mut, value.as_ref(), &registry);
    Ok(json!({ "entity": entity.to_bits(), "component": component }))
}

/// `scene.remove_component`.
fn remove_component_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` (type name) is required"))?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let resolved = resolve_component(&registry, component)?;
    let type_id = resolved.registration.type_info().type_id();
    let removed = world.entity(entity).contains_type_id(type_id);
    if removed {
        let mut entity_mut = world.entity_mut(entity);
        resolved.data.remove(&mut entity_mut);
    }
    Ok(json!({ "entity": entity.to_bits(), "component": component, "removed": removed }))
}

/// The entities the edits seen so far in phase 1 despawn, and the parents their reparents set
/// (ADR-5597). `despawn` takes the whole subtree (`despawn_tree`), so a later edit naming a
/// descendant of a despawned entity must be refused as well; the subtree is taken from the
/// hierarchy as the earlier reparents of the same transaction leave it.
#[derive(Default)]
struct PlannedHierarchy {
    /// Entities an earlier edit despawns, descendants included.
    despawned: std::collections::HashSet<Entity>,
    /// `child -> new parent` (`None`: made a root) from earlier `reparent` edits.
    reparented: std::collections::HashMap<Entity, Option<Entity>>,
}

impl PlannedHierarchy {
    /// The planned parent of `entity`: an earlier reparent's, else the world's `ChildOf`.
    fn parent_of(&self, world: &World, entity: Entity) -> Option<Entity> {
        match self.reparented.get(&entity) {
            Some(parent) => *parent,
            None => world.get::<ChildOf>(entity).map(ChildOf::parent),
        }
    }

    /// Records a validated `despawn` or `reparent` edit.
    fn record(&mut self, world: &World, edit: &Value, op: &str) {
        let Ok(entity) = entity_from(edit) else {
            return;
        };
        match op {
            "reparent" => {
                let parent = edit
                    .get("parent")
                    .and_then(Value::as_u64)
                    .and_then(Entity::try_from_bits);
                self.reparented.insert(entity, parent);
            }
            "despawn" => {
                // Everything whose planned ancestor chain reaches `entity`: the world's
                // descendants that no earlier reparent moved away, plus entities an earlier
                // reparent moved under the subtree.
                let mut stack = vec![entity];
                while let Some(node) = stack.pop() {
                    if !self.despawned.insert(node) {
                        continue;
                    }
                    if let Some(children) = world.get::<Children>(node) {
                        stack.extend(
                            children
                                .iter()
                                .filter(|child| self.parent_of(world, *child) == Some(node)),
                        );
                    }
                    stack.extend(
                        self.reparented
                            .iter()
                            .filter(|(_, parent)| **parent == Some(node))
                            .map(|(child, _)| *child),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Phase 1 refuses an edit that names an entity (or a reparent/spawn parent) an earlier edit
/// despawns, directly or as a descendant of the despawned entity. The world is unchanged. A bad
/// id falls through to the per-op validator.
fn refuse_if_earlier_despawn(
    planned: &PlannedHierarchy,
    index: usize,
    edit: &Value,
    op: &str,
) -> Result<(), RpcError> {
    let target = match op {
        "set_property" | "add_component" | "remove_component" | "despawn" | "reparent" => {
            entity_from(edit).ok()
        }
        _ => None,
    };
    if let Some(entity) = target
        && planned.despawned.contains(&entity)
    {
        return Err(RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0014",
            format!(
                "edits[{index}] ({op}) names entity {} which an earlier edit despawns",
                entity.to_bits()
            ),
            "fix the edit; nothing in this transaction was applied",
        ));
    }
    if matches!(op, "reparent" | "spawn")
        && let Some(bits) = edit.get("parent").and_then(Value::as_u64)
        && let Some(parent) = Entity::try_from_bits(bits)
        && planned.despawned.contains(&parent)
    {
        return Err(RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0014",
            format!("edits[{index}] ({op}) names parent {bits} which an earlier edit despawns"),
            "fix the edit; nothing in this transaction was applied",
        ));
    }
    Ok(())
}

/// `scene.apply_edits`: run several scene edits in order, validating everything possible
/// before applying any of them (no runtime undo — see the module docs).
fn apply_edits_raw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let edits = crate::params::opt_array(params, "edits")?
        .filter(|a| !a.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`edits` (a non-empty array of edit objects) is required"))?;
    let dry_run = dry_run_flag(params)?;

    // Phase 1: validate every edit without touching the world.
    let mut planned = PlannedHierarchy::default();
    for (index, edit) in edits.iter().enumerate() {
        let op = crate::params::opt_str(edit, "op")?.unwrap_or("");
        refuse_if_earlier_despawn(&planned, index, edit, op)?;
        let validated = match op {
            "set_property" => validate_set_property(world, edit, index, edits),
            "add_component" => validate_add(world, edit).map(drop),
            "remove_component" => validate_remove(world, edit).map(drop),
            "spawn" => validate_spawn(world, edit),
            "despawn" => validate_despawn(world, edit),
            "reparent" => validate_reparent(world, edit),
            other => {
                return Err(RpcError::invalid_params(format!(
                    "edits[{index}]: unknown op {other:?} (set_property, add_component, \
                     remove_component, spawn, despawn, reparent)"
                )));
            }
        };
        if let Err(error) = validated {
            return Err(RpcError::new(
                crate::protocol::INVALID_PARAMS,
                "F0014",
                format!("edits[{index}] ({op}) does not validate: {}", error.message),
                "fix the edit; nothing in this transaction was applied",
            ));
        }
        planned.record(world, edit, op);
    }
    if dry_run {
        return Ok(json!({ "applied": false, "validated": edits.len() }));
    }

    // Phase 2: apply for real. A failure here means the world changed mid-transaction (e.g. an
    // entity removed by an earlier edit of this same transaction); the response says how far
    // it got.
    let mut results = Vec::new();
    let mut applied = 0;
    for (index, edit) in edits.iter().enumerate() {
        let op = crate::params::opt_str(edit, "op")?.unwrap_or("");
        let outcome = match op {
            "set_property" => set_property(world, edit),
            "add_component" => add_component(world, edit),
            "remove_component" => remove_component(world, edit),
            "spawn" => spawn(world, edit),
            "despawn" => despawn(world, edit),
            "reparent" => reparent(world, edit),
            _ => unreachable!("validated above"), // invariant: every op was whitelisted in phase 1
        };
        match outcome {
            Ok(value) => {
                applied += 1;
                results.push(json!({ "op": op, "ok": true, "result": value }));
            }
            Err(error) => {
                results.push(json!({ "op": op, "ok": false, "error": error.message }));
                return Ok(json!({
                    "applied": applied,
                    "applied_all": false,
                    "failed_at": index,
                    "results": results,
                    "note": "earlier edits were applied; the protocol has no runtime undo",
                }));
            }
        }
    }
    Ok(json!({ "applied": applied, "applied_all": true, "results": results }))
}

/// Validates a `set_property` edit. A component added by an earlier edit of the same
/// transaction is accepted (the apply phase re-checks presence), and the value is checked
/// against the field's type through the entity's instance or the type's default.
fn validate_set_property(
    world: &mut World,
    edit: &Value,
    index: usize,
    edits: &[Value],
) -> Result<(), RpcError> {
    let entity = entity_from(edit)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let component = edit
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` is required"))?;
    let path = crate::params::opt_str(edit, "path")?.unwrap_or("");
    let value = edit
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let resolved = resolve_component(&registry, component)?;
    let type_id = resolved.registration.type_info().type_id();
    let has_now = world.entity(entity).contains_type_id(type_id);
    let added_earlier = edits[..index].iter().any(|other| {
        other.get("op").and_then(Value::as_str) == Some("add_component")
            && other.get("component").and_then(Value::as_str) == Some(component)
            && other.get("entity").and_then(Value::as_u64) == Some(entity.to_bits())
    });
    if !has_now && !added_earlier {
        return Err(RpcError::not_found(
            format!("entity has no {component}"),
            "add the component first (an earlier add_component edit in this transaction counts)",
        ));
    }
    // Validate the value against the field's type, using an instance when one exists.
    let instance: Option<Box<dyn regia_ecs::reflect::Reflect>> = if has_now {
        resolved
            .data
            .reflect(world.entity(entity))
            .and_then(|value| value.reflect_clone().ok())
    } else {
        use regia_ecs::reflect::std_traits::ReflectDefault;
        resolved
            .registration
            .data::<ReflectDefault>()
            .map(|data| data.default())
    };
    let Some(instance) = instance else {
        return Ok(()); // cannot type-check without an instance; the apply phase still will
    };
    let field: &dyn PartialReflect = if path.is_empty() {
        instance.as_partial_reflect()
    } else {
        instance.reflect_path(path).map_err(|e| {
            RpcError::not_found(
                format!("no field `{path}` in {component}: {e}"),
                "see types.describe for the component's fields",
            )
        })?
    };
    let field_type = field
        .get_represented_type_info()
        .map(|info| info.type_id())
        .and_then(|t| registry.get(t))
        .ok_or_else(|| RpcError::internal(format!("field `{path}` has no registered type")))?;
    TypedReflectDeserializer::new(field_type, &registry)
        .deserialize(value)
        .map_err(|e| {
            RpcError::invalid_params(format!(
                "value does not fit `{path}` ({}): {e}",
                field_type.type_info().type_path()
            ))
        })?;
    Ok(())
}

/// Validates a `spawn` edit: every component resolves and its value deserialises.
fn validate_spawn(world: &mut World, edit: &Value) -> Result<(), RpcError> {
    let components = edit.get("components").cloned().unwrap_or_else(|| json!({}));
    let Some(map) = components.as_object() else {
        return Err(RpcError::invalid_params(
            "`components` must be an object of { \"Type\": value }",
        ));
    };
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    build_components(&registry, map).map(drop)?;
    if let Some(parent) = crate::params::opt_u64(edit, "parent")? {
        let parent = Entity::try_from_bits(parent)
            .ok_or_else(|| RpcError::invalid_params(format!("{parent} is not an entity id")))?;
        require_entity(world, parent)?;
    }
    Ok(())
}

/// Validates a `despawn` edit without touching the world: the entity must exist.
fn validate_despawn(world: &World, edit: &Value) -> Result<(), RpcError> {
    let entity = entity_from(edit)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)
}

/// Validates a `reparent` edit: both entities exist and the move is not a self-parenting.
fn validate_reparent(world: &mut World, edit: &Value) -> Result<(), RpcError> {
    let entity = entity_from(edit)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    if let Some(parent) = crate::params::opt_u64(edit, "parent")? {
        let parent = Entity::try_from_bits(parent)
            .ok_or_else(|| RpcError::invalid_params(format!("{parent} is not an entity id")))?;
        require_entity(world, parent)?;
        if would_cycle(world, entity, parent) {
            return Err(RpcError::invalid_params(
                "an entity cannot be its own parent or the parent of one of its ancestors",
            ));
        }
    }
    Ok(())
}

/// True when making `parent` the parent of `entity` would close a `ChildOf` cycle: `parent` is
/// `entity` itself or one of its descendants (`entity` is on `parent`'s ancestor chain).
fn would_cycle(world: &World, entity: Entity, parent: Entity) -> bool {
    let mut cursor = parent;
    // The visited set only matters for a world that already holds a cycle; it keeps this walk
    // from hanging there.
    let mut seen = std::collections::HashSet::new();
    loop {
        if cursor == entity || !seen.insert(cursor) {
            return true;
        }
        match world.get::<ChildOf>(cursor) {
            Some(child_of) => cursor = child_of.parent(),
            None => return false,
        }
    }
}

/// Validates an `add_component` edit: resolves the type and deserialises the value.
fn validate_add(world: &mut World, edit: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(edit)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let component = edit
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` is required"))?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let resolved = resolve_component(&registry, component)?;
    if let Some(value) = edit.get("value") {
        deserialize_component(&resolved, value, &registry)?;
    }
    Ok(json!({ "entity": entity.to_bits(), "component": component }))
}

/// Validates a `remove_component` edit: entity and component type exist.
fn validate_remove(world: &mut World, edit: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(edit)?;
    require_entity(world, entity)?;
    require_not_feed_owned(world, entity)?;
    let component = edit
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`component` is required"))?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    resolve_component(&registry, component)?;
    Ok(json!({ "entity": entity.to_bits(), "component": component }))
}

/// `scene.serialize`: the whole scene as canonical RON (the `regia://scene/current`
/// resource).
pub(crate) fn serialize(world: &mut World) -> Result<Value, RpcError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut codec = ReflectCodec::new(&registry);
    let mut q = world.query::<Entity>();
    let mut entities: Vec<Entity> = q.iter(world).collect();
    entities.sort_by_key(|e| e.to_bits());
    let mut count = 0usize;
    let mut components = 0usize;
    let mut ron = String::new();
    ron.push_str("(\n    entities: [\n");
    for entity in entities {
        let record_start = ron.len();
        let components_before = components;
        let bits = entity.to_bits();
        let name = entity_name(world, entity).unwrap_or_default();
        let parent = world.get::<ChildOf>(entity).map(|c| c.parent().to_bits());
        ron.push_str(&format!(
            "        // entity {bits} \"{}\"{}\n        (\n",
            name,
            parent.map(|p| format!(", parent {p}")).unwrap_or_default()
        ));
        for (short, _, type_id) in components_of(world, &registry, entity) {
            if is_relationship(&short) {
                continue;
            }
            let Some(data) = type_id
                .and_then(|t| registry.get(t))
                .and_then(|r| r.data::<ReflectComponent>())
                .and_then(|rc| rc.reflect(world.entity(entity)))
            else {
                continue;
            };
            components += 1;
            match codec.to_ron(data.as_partial_reflect()) {
                Ok(text) => {
                    for line in text.lines() {
                        ron.push_str("            ");
                        ron.push_str(line);
                        ron.push('\n');
                    }
                }
                Err(err) => {
                    ron.push_str(&format!("            // {short}: {err}\n"));
                }
            }
        }
        // Bevy 0.20 keeps one backing entity per resource type, even after removal. A lazy
        // panel therefore grows storage without creating scene content. Suppress only those
        // opaque resource rows: actual empty entities and reflected resources (LevelLayers,
        // settings, etc.) must remain visible to leak and undo checks (ADR-7956).
        if components == components_before
            && world
                .get::<regia_ecs::ecs::resource::IsResource>(entity)
                .is_some()
        {
            ron.truncate(record_start);
            continue;
        }
        ron.push_str("        ),\n");
        count += 1;
    }
    ron.push_str("    ],\n)\n");
    Ok(json!({ "ron": ron, "entities": count, "components": components }))
}

/// A native scene-edit handler. Hosts wrap it without reimplementing runtime validation.
pub type SceneEditHandler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Optional host hook for scene authoring calls. Installed only in the editor's edit world;
/// game and play worlds keep the native, undo-free behavior and no editor dependency.
#[derive(Resource, Clone, Copy)]
pub struct SceneEditHook {
    /// Runs a native handler and records its changes in the host's history.
    pub edit: fn(&mut World, &str, &Value, SceneEditHandler) -> Result<Value, RpcError>,
}

/// Calls a native mutation directly unless the host installed a scene-edit hook.
pub(crate) fn edit(
    world: &mut World,
    method: &str,
    params: &Value,
    native: SceneEditHandler,
) -> Result<Value, RpcError> {
    match world.get_resource::<SceneEditHook>().copied() {
        Some(hook) => (hook.edit)(world, method, params, native),
        None => native(world, params),
    }
}

/// Routes `scene.set_property` through optional host history before native mutation.
pub(crate) fn set_property(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.set_property", params, set_property_raw)
}

/// Routes `scene.spawn` through optional host history before native mutation.
pub(crate) fn spawn(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.spawn", params, spawn_raw)
}

/// Routes `scene.despawn` through optional host history before native mutation.
pub(crate) fn despawn(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.despawn", params, despawn_raw)
}

/// Routes `scene.duplicate` through optional host history before native mutation.
pub(crate) fn duplicate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.duplicate", params, duplicate_raw)
}

/// Routes `scene.reparent` through optional host history before native mutation.
pub(crate) fn reparent(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.reparent", params, reparent_raw)
}

/// Routes `scene.add_component` through optional host history before native mutation.
pub(crate) fn add_component(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.add_component", params, add_component_raw)
}

/// Routes `scene.remove_component` through optional host history before native mutation.
pub(crate) fn remove_component(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.remove_component", params, remove_component_raw)
}

/// Routes `scene.apply_edits` through optional host history before native mutation.
pub(crate) fn apply_edits(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    edit(world, "scene.apply_edits", params, apply_edits_raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::{Quat, Vec3};
    use regia_scene::Transform;

    /// A component the agent can spawn and edit.
    #[derive(Component, Reflect, Clone, Debug, PartialEq, Default)]
    #[reflect(Component, Default)]
    /// A marker with a number.
    struct Marker {
        /// A number.
        value: f32,
    }

    #[test]
    fn name_selectors_find_entities_named_with_the_scene_name_component() {
        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        let ecs_named = world.spawn(Name::new("ecs_named")).id();
        let scene_named = world
            .spawn(regia_scene::components::Name::new("main_camera"))
            .id();
        assert_eq!(entity_name(&world, ecs_named).as_deref(), Some("ecs_named"));
        assert_eq!(entity_name(&world, scene_named).as_deref(), Some("main_camera"));
        assert_eq!(
            resolve_entity(&world, &json!({ "name": "main_camera" })),
            Ok(scene_named)
        );
        assert_eq!(
            resolve_entity(&world, &json!({ "name": "main_*" })),
            Ok(scene_named)
        );
        assert!(resolve_entity(&world, &json!({ "name": "missing" })).is_err());
    }

    #[test]
    fn a_component_named_with_the_legacy_crate_names_resolves() {
        let mut registry = TypeRegistry::default();
        registry.register::<Transform>();
        let current = <Transform as regia_ecs::reflect::TypePath>::type_path();
        // An agent script written before the rename to Regia Engine (ADR-1429).
        let legacy = regia_core::brand::fingerprint_type_path(current);
        let found = registration(&registry, &legacy).expect("legacy path resolves");
        assert_eq!(found.type_info().type_path(), current);
        assert!(registration(&registry, "no_crate::Missing").is_err());
    }

    #[test]
    fn resource_entities_are_not_scene_entities() {
        let mut app = app();
        let resource = app
            .world_mut()
            .query_filtered::<Entity, With<regia_ecs::ecs::resource::IsResource>>()
            .iter(app.world())
            .next()
            .expect("MinimalPlugins inserts resources");
        let bits = resource.to_bits();
        for call in [despawn, duplicate, reparent] {
            let error = call(app.world_mut(), &json!({ "entity": bits })).expect_err("refused");
            assert_eq!(error.code, crate::protocol::NOT_FOUND);
        }
        assert!(
            app.world().get_entity(resource).is_ok(),
            "the resource entity survives"
        );
        assert!(app.world().get_resource::<Time>().is_some());
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.register_type::<Transform>();
        app.register_type::<Marker>();
        app.register_type::<Name>();
        app
    }

    fn spawn_named(world: &mut World, name: &str, translation: [f32; 3]) -> Entity {
        world
            .spawn((
                Name::new(name.to_string()),
                Transform {
                    translation: translation.into(),
                    rotation: Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
            ))
            .id()
    }

    #[test]
    fn query_filters_by_name_component_and_space() {
        let mut app = app();
        {
            let world = app.world_mut();
            spawn_named(world, "Rock A", [0.0, 0.0, 0.0]);
            spawn_named(world, "Rock B", [10.0, 0.0, 0.0]);
            spawn_named(world, "Tree", [1.0, 0.0, 0.0]);
        }
        let world = app.world_mut();

        let rocks = query_entities(world, &json!({ "name": "Rock*" })).expect("glob");
        assert_eq!(rocks["count"], 2);

        let transforms = query_entities(world, &json!({ "component": "Transform" })).expect("component");
        assert_eq!(transforms["count"], 3);
        assert!(
            transforms["entities"][0]["translation"].is_array(),
            "translations are reported: {}",
            transforms["entities"][0]
        );

        let near = query_entities(
            world,
            &json!({ "component": "Transform", "near": { "point": [0.0, 0.0, 0.0], "radius": 2.0 } }),
        )
        .expect("near");
        assert_eq!(near["count"], 2, "Rock A and Tree are near the origin: {near}");

        let in_box = query_entities(world, &json!({ "component": "Transform", "box": { "min": [9.0, -1.0, -1.0], "max": [11.0, 1.0, 1.0] } })).expect("box");
        assert_eq!(in_box["count"], 1);
    }

    #[test]
    fn get_entity_reports_components_and_values() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Probe", [1.0, 2.0, 3.0]);
        app.world_mut().entity_mut(entity).insert(Marker { value: 7.5 });

        let got = get_entity(app.world_mut(), &json!({ "entity": entity.to_bits() })).expect("gets");
        assert_eq!(got["name"], "Probe");
        assert_eq!(got["components"]["Marker"]["value"]["value"], 7.5);
        let transform = &got["components"]["Transform"]["value"];
        assert_eq!(
            transform["translation"][0], 1.0,
            "glam Vec3 serializes as a sequence: {transform}"
        );
        assert_eq!(
            got["components"]["Transform"]["type"],
            <Transform as regia_ecs::reflect::TypePath>::type_path(),
            "the type string is the module path scene.get_entity returns"
        );

        let filtered = get_entity(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "components": ["Marker"] }),
        )
        .expect("gets");
        assert!(filtered["components"].as_object().map(|m| m.len()) == Some(1));

        let missing = get_entity(app.world_mut(), &json!({ "entity": 1u64 << 40 }));
        assert!(missing.is_err());
    }

    #[test]
    fn set_property_rejects_non_boolean_dry_run_without_writing() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Mover", [0.0, 0.0, 0.0]);
        for bad in [json!("true"), json!(1), json!([true])] {
            let err = set_property(
                app.world_mut(),
                &json!({
                    "entity": entity.to_bits(),
                    "component": "Transform",
                    "path": "translation.y",
                    "value": 5.0,
                    "dry_run": bad,
                }),
            )
            .expect_err("a non-boolean dry_run is rejected");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
            let y = app
                .world()
                .get::<Transform>(entity)
                .expect("Transform")
                .translation
                .y;
            assert_eq!(y, 0.0, "nothing written for {bad}");
        }
        let err = apply_edits(
            app.world_mut(),
            &json!({ "dry_run": "yes", "edits": [{ "op": "despawn", "entity": entity.to_bits() }] }),
        )
        .expect_err("apply_edits rejects it too");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        assert!(
            app.world().get::<Transform>(entity).is_some(),
            "entity still exists"
        );
    }

    #[test]
    fn set_property_round_trips_and_dry_run_does_not_apply() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Mover", [0.0, 0.0, 0.0]);

        let dry = set_property(
            app.world_mut(),
            &json!({
                "entity": entity.to_bits(),
                "component": "Transform",
                "path": "translation.y",
                "value": 5.0,
                "dry_run": true,
            }),
        )
        .expect("dry run");
        assert_eq!(dry["applied"], false);
        assert_eq!(dry["before"], 0.0);
        assert_eq!(dry["after"], 5.0);
        assert_eq!(
            app.world()
                .get::<Transform>(entity)
                .expect("Transform present")
                .translation
                .y,
            0.0
        );

        let applied = set_property(
            app.world_mut(),
            &json!({
                "entity": entity.to_bits(),
                "component": "Transform",
                "path": "translation",
                "value": [1.0, 5.0, -2.0],
            }),
        )
        .expect("applies");
        assert_eq!(applied["applied"], true);
        assert_eq!(
            app.world()
                .get::<Transform>(entity)
                .expect("Transform present")
                .translation
                .y,
            5.0
        );

        // Bad field and bad value are clean errors.
        assert!(set_property(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "component": "Transform", "path": "nope", "value": 1.0 }),
        )
        .is_err());
        assert!(set_property(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "component": "Transform", "path": "translation", "value": "up" }),
        )
        .is_err());
    }

    /// `scene.despawn` on the top of a 100,000-link chain removes the whole chain on the test's own
    /// (default) stack: the despawn is iterative (`despawn_tree`, ADR-2306), where bevy's recursive
    /// despawn overflowed a 2 MiB Windows thread at 5,000 links.
    #[test]
    fn scene_despawn_of_a_deep_chain_runs_on_the_default_stack() {
        let mut world = World::new();
        let root = world.spawn(Transform::IDENTITY).id();
        let mut leaf = root;
        for _ in 0..100_000 {
            leaf = world.spawn((Transform::IDENTITY, ChildOf(leaf))).id();
        }
        let out = despawn(&mut world, &json!({ "entity": root.to_bits() })).expect("despawns");
        assert_eq!(out["despawned"], true);
        assert!(world.get_entity(leaf).is_err());
        let mut left = world.query::<&Transform>();
        assert_eq!(left.iter(&world).count(), 0);
    }

    #[test]
    fn spawn_despawn_reparent_and_duplicate() {
        let mut app = app();
        let parent = spawn_named(app.world_mut(), "Parent", [0.0; 3]);

        let spawned = spawn(
            app.world_mut(),
            &json!({
                "name": "Child",
                "parent": parent.to_bits(),
                "components": {
                    "Marker": { "value": 2.0 },
                    "Transform": {
                        "translation": [1.0, 2.0, 3.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0],
                    },
                },
            }),
        )
        .expect("spawns");
        let child = Entity::from_bits(spawned["entity"].as_u64().expect("id"));
        assert_eq!(
            app.world().get::<Name>(child).expect("Name present").as_str(),
            "Child"
        );
        assert_eq!(
            app.world()
                .get::<ChildOf>(child)
                .expect("ChildOf present")
                .parent(),
            parent
        );
        assert_eq!(
            app.world().get::<Marker>(child).expect("Marker present").value,
            2.0
        );

        // Duplicate copies reflected components and the parent link, not relationships.
        let dup = duplicate(app.world_mut(), &json!({ "entity": child.to_bits() })).expect("duplicates");
        let copy = Entity::from_bits(dup["entity"].as_u64().expect("id"));
        assert_eq!(
            app.world().get::<Marker>(copy).expect("Marker present").value,
            2.0
        );
        assert_eq!(
            app.world()
                .get::<ChildOf>(copy)
                .expect("ChildOf present")
                .parent(),
            parent
        );
        assert_eq!(dup["copied_components"], 3, "Name, Transform, Marker: {dup}");

        // Reparent under the copy, then detach.
        reparent(
            app.world_mut(),
            &json!({ "entity": child.to_bits(), "parent": copy.to_bits() }),
        )
        .expect("reparents");
        assert_eq!(
            app.world()
                .get::<ChildOf>(child)
                .expect("ChildOf present")
                .parent(),
            copy
        );
        reparent(
            app.world_mut(),
            &json!({ "entity": child.to_bits(), "parent": null }),
        )
        .expect("detaches");
        assert!(app.world().get::<ChildOf>(child).is_none());

        // A detached entity survives its former parent's despawn (hierarchy hooks keep
        // Children in sync when ChildOf is removed).
        assert!(despawn(app.world_mut(), &json!({ "entity": copy.to_bits() })).is_ok());
        assert!(app.world().get_entity(copy).is_err());
        assert!(
            app.world().get_entity(child).is_ok(),
            "the detached child survives the copy's despawn"
        );
        assert!(app.world().get_entity(parent).is_ok(), "grandparent survives");

        // Despawning a parent despawns the children still attached to it (ChildOf hooks).
        let attached = spawn(
            app.world_mut(),
            &json!({ "name": "Attached", "parent": parent.to_bits() }),
        )
        .expect("spawns");
        let attached = Entity::from_bits(attached["entity"].as_u64().expect("id"));
        assert!(despawn(app.world_mut(), &json!({ "entity": parent.to_bits() })).is_ok());
        assert!(app.world().get_entity(parent).is_err());
        assert!(
            app.world().get_entity(attached).is_err(),
            "the attached child went with the parent"
        );

        // Self-parenting is rejected (the parent is gone; use the surviving child).
        assert!(
            reparent(
                app.world_mut(),
                &json!({ "entity": child.to_bits(), "parent": child.to_bits() })
            )
            .is_err()
        );
    }

    #[test]
    fn add_and_remove_component() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Blank", [0.0; 3]);

        add_component(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "component": "Marker", "value": { "value": 3.0 } }),
        )
        .expect("adds");
        assert_eq!(
            app.world().get::<Marker>(entity).expect("Marker present").value,
            3.0
        );

        // Default value when no value is passed.
        let second = spawn_named(app.world_mut(), "Default", [0.0; 3]);
        add_component(
            app.world_mut(),
            &json!({ "entity": second.to_bits(), "component": "Marker" }),
        )
        .expect("adds default");
        assert_eq!(
            app.world().get::<Marker>(second).expect("Marker present").value,
            0.0,
            "derived default"
        );

        let removed = remove_component(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "component": "Marker" }),
        )
        .expect("removes");
        assert_eq!(removed["removed"], true);
        assert!(app.world().get::<Marker>(entity).is_none());
        let again = remove_component(
            app.world_mut(),
            &json!({ "entity": entity.to_bits(), "component": "Marker" }),
        )
        .expect("removes");
        assert_eq!(again["removed"], false, "removing twice reports false");
    }

    #[test]
    fn apply_edits_is_atomic_on_validation_failures() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Target", [0.0; 3]);
        let bits = entity.to_bits();

        // A bad edit anywhere → nothing applies.
        let bad = apply_edits(
            app.world_mut(),
            &json!({ "edits": [
                { "op": "set_property", "entity": bits, "component": "Transform", "path": "translation.y", "value": 2.0 },
                { "op": "set_property", "entity": bits, "component": "Nope", "value": 1.0 },
            ]}),
        );
        let err = bad.expect_err("rejects");
        assert!(err.message.contains("edits[1]"), "{err:?}");
        assert_eq!(
            app.world()
                .get::<Transform>(entity)
                .expect("Transform present")
                .translation
                .y,
            0.0,
            "nothing applied"
        );

        // A good transaction applies in order and reports per-edit results.
        let good = apply_edits(
            app.world_mut(),
            &json!({ "edits": [
                { "op": "add_component", "entity": bits, "component": "Marker", "value": { "value": 9.0 } },
                { "op": "set_property", "entity": bits, "component": "Marker", "path": "value", "value": 4.0 },
                { "op": "spawn", "name": "Added" },
            ]}),
        )
        .expect("applies");
        assert_eq!(good["applied_all"], true);
        assert_eq!(good["applied"], 3);
        assert_eq!(
            app.world().get::<Marker>(entity).expect("Marker present").value,
            4.0
        );
        assert!(query_entities(app.world_mut(), &json!({ "name": "Added" })).expect("query")["count"] == 1);

        // dry_run validates but does not apply.
        let dry = apply_edits(
            app.world_mut(),
            &json!({ "dry_run": true, "edits": [
                { "op": "set_property", "entity": bits, "component": "Marker", "path": "value", "value": 8.0 },
            ]}),
        )
        .expect("validates");
        assert_eq!(dry["applied"], false);
        assert_eq!(
            app.world().get::<Marker>(entity).expect("Marker present").value,
            4.0
        );
    }

    /// An edit that names an entity an earlier edit despawns fails validation. Nothing is applied.
    #[test]
    fn apply_edits_rejects_an_edit_after_a_despawn_of_its_target() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Target", [0.0; 3]);
        let bits = entity.to_bits();
        let err = apply_edits(
            app.world_mut(),
            &json!({ "edits": [
                { "op": "despawn", "entity": bits },
                { "op": "set_property", "entity": bits, "component": "Transform", "path": "translation.y", "value": 2.0 },
            ]}),
        )
        .expect_err("rejects");
        assert!(err.message.contains("edits[1]"), "{err:?}");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0014")
        );
        assert!(app.world().get_entity(entity).is_ok(), "nothing applied");
        assert_eq!(
            app.world()
                .get::<Transform>(entity)
                .expect("Transform")
                .translation
                .y,
            0.0
        );
    }

    /// corr-agent (ADR-5597): `despawn` takes the whole subtree, so an edit that names a child
    /// of an earlier-despawned entity used to pass phase 1, then fail in phase 2 after the
    /// despawn had been applied (`applied_all: false`, parent gone).
    #[test]
    fn apply_edits_rejects_an_edit_on_a_child_of_a_despawned_parent() {
        let mut app = app();
        let parent = spawn_named(app.world_mut(), "Parent", [0.0; 3]);
        let child = spawn_named(app.world_mut(), "Child", [0.0; 3]);
        let grandchild = spawn_named(app.world_mut(), "Grandchild", [0.0; 3]);
        app.world_mut().entity_mut(child).insert(ChildOf(parent));
        app.world_mut().entity_mut(grandchild).insert(ChildOf(child));
        for (what, edit) in [
            (
                "set_property on the grandchild",
                json!({ "op": "set_property", "entity": grandchild.to_bits(), "component": "Transform", "path": "translation.y", "value": 2.0 }),
            ),
            (
                "spawn under the child",
                json!({ "op": "spawn", "name": "New", "parent": child.to_bits() }),
            ),
        ] {
            let err = apply_edits(
                app.world_mut(),
                &json!({ "edits": [{ "op": "despawn", "entity": parent.to_bits() }, edit] }),
            )
            .expect_err(what);
            assert!(err.message.contains("edits[1]"), "{what}: {err:?}");
            assert!(app.world().get_entity(parent).is_ok(), "{what}: nothing applied");
            assert!(
                app.world().get_entity(grandchild).is_ok(),
                "{what}: nothing applied"
            );
        }
    }

    /// The planned hierarchy follows earlier reparents: a child moved out before its parent's
    /// despawn survives it and may be edited; one moved under the despawned entity may not.
    #[test]
    fn apply_edits_follows_earlier_reparents_into_and_out_of_a_despawned_subtree() {
        let mut app = app();
        let parent = spawn_named(app.world_mut(), "Parent", [0.0; 3]);
        let child = spawn_named(app.world_mut(), "Child", [0.0; 3]);
        let loose = spawn_named(app.world_mut(), "Loose", [0.0; 3]);
        app.world_mut().entity_mut(child).insert(ChildOf(parent));
        let moved_in = apply_edits(
            app.world_mut(),
            &json!({ "edits": [
                { "op": "reparent", "entity": loose.to_bits(), "parent": parent.to_bits() },
                { "op": "despawn", "entity": parent.to_bits() },
                { "op": "set_property", "entity": loose.to_bits(), "component": "Transform", "path": "translation.y", "value": 2.0 },
            ]}),
        )
        .expect_err("moved under the despawned parent");
        assert!(moved_in.message.contains("edits[2]"), "{moved_in:?}");
        assert!(app.world().get_entity(parent).is_ok(), "nothing applied");

        let moved_out = apply_edits(
            app.world_mut(),
            &json!({ "edits": [
                { "op": "reparent", "entity": child.to_bits() },
                { "op": "despawn", "entity": parent.to_bits() },
                { "op": "set_property", "entity": child.to_bits(), "component": "Transform", "path": "translation.y", "value": 2.0 },
            ]}),
        )
        .expect("moved out first");
        assert_eq!(moved_out["applied_all"], true, "{moved_out}");
        assert!(app.world().get_entity(parent).is_err());
        assert_eq!(
            app.world()
                .get::<Transform>(child)
                .expect("Transform")
                .translation
                .y,
            2.0
        );
    }

    #[test]
    fn dry_run_despawn_leaves_the_world_untouched() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Keep", [0.0; 3]);
        let dry = apply_edits(
            app.world_mut(),
            &json!({ "dry_run": true, "edits": [{ "op": "despawn", "entity": entity.to_bits() }] }),
        )
        .expect("validates");
        assert_eq!(dry["applied"], false);
        assert!(app.world().get_entity(entity).is_ok(), "dry run must not despawn");

        let real = apply_edits(
            app.world_mut(),
            &json!({ "edits": [{ "op": "despawn", "entity": entity.to_bits() }] }),
        )
        .expect("applies");
        assert_eq!(real["applied_all"], true, "{real}");
        assert!(app.world().get_entity(entity).is_err());
    }

    #[test]
    fn reparent_rejects_cycles() {
        let mut app = app();
        let a = spawn_named(app.world_mut(), "A", [0.0; 3]);
        let b = spawn_named(app.world_mut(), "B", [0.0; 3]);
        reparent(
            app.world_mut(),
            &json!({ "entity": b.to_bits(), "parent": a.to_bits() }),
        )
        .expect("b under a");
        let cycle = reparent(
            app.world_mut(),
            &json!({ "entity": a.to_bits(), "parent": b.to_bits() }),
        );
        assert!(cycle.is_err(), "a under its own child must be rejected");
        assert!(app.world().get::<ChildOf>(a).is_none());
        let validated = apply_edits(
            app.world_mut(),
            &json!({ "dry_run": true, "edits": [
                { "op": "reparent", "entity": a.to_bits(), "parent": b.to_bits() },
            ]}),
        );
        assert!(validated.is_err(), "validation rejects the cycle too");
    }

    #[test]
    fn query_entities_limit_zero_is_an_empty_page() {
        let mut app = app();
        spawn_named(app.world_mut(), "One", [0.0; 3]);
        spawn_named(app.world_mut(), "Two", [0.0; 3]);
        let page = query_entities(app.world_mut(), &json!({ "limit": 0 })).expect("query");
        assert_eq!(page["count"], 0);
        let page = query_entities(app.world_mut(), &json!({ "limit": 1 })).expect("query");
        assert_eq!(page["count"], 1);
    }

    #[test]
    fn queries_and_name_resolution_are_ordered_by_entity_id() {
        let mut app = app();
        let mut spawned = [
            spawn_named(app.world_mut(), "Enemy1", [0.0; 3]),
            spawn_named(app.world_mut(), "Enemy2", [0.0; 3]),
            spawn_named(app.world_mut(), "Enemy3", [0.0; 3]),
        ];
        // Entity id order is `Entity`'s own (its bits), which is not spawn order in bevy_ecs 0.20.
        spawned.sort();
        let [first, second, third] = spawned;
        // Moving the lowest id to another archetype must not change which entity wins.
        app.world_mut().entity_mut(first).insert(Marker { value: 1.0 });
        let world = app.world_mut();
        assert_eq!(
            resolve_entity(world, &json!({ "name": "Enemy*" })).ok(),
            Some(first)
        );
        let all = query_entities(world, &json!({ "name": "Enemy*" })).expect("query");
        let ids: Vec<u64> = all["entities"]
            .as_array()
            .expect("list")
            .iter()
            .filter_map(|e| e["entity"].as_u64())
            .collect();
        assert_eq!(ids, vec![first.to_bits(), second.to_bits(), third.to_bits()]);
        let limited = query_entities(world, &json!({ "name": "Enemy*", "limit": 2 })).expect("query");
        assert_eq!(limited["entities"][0]["entity"], first.to_bits());
        assert_eq!(limited["entities"][1]["entity"], second.to_bits());
    }

    #[test]
    fn resolve_entity_accepts_ids_names_and_components() {
        let mut app = app();
        let entity = spawn_named(app.world_mut(), "Player*", [0.0; 3]);
        let world = app.world_mut();

        assert_eq!(resolve_entity(world, &json!(entity.to_bits())).ok(), Some(entity));
        assert_eq!(
            resolve_entity(world, &json!({ "name": "Player*" })).ok(),
            Some(entity)
        );
        assert_eq!(
            resolve_entity(world, &json!({ "component": "Marker" })).err(),
            Some(
                "no entity matches {\"component\":\"Marker\"} — use scene.query_entities to see what exists"
                    .to_string()
            )
        );
        assert!(resolve_entity(world, &json!("not-an-entity")).is_err());
        assert!(resolve_entity(world, &json!(4_294_967_296u64)).is_err());
    }

    #[test]
    fn smoke_serialize_keeps_empty_entities_but_not_resource_storage() {
        #[derive(Resource)]
        struct PanelState;
        let mut app = app();
        let empty = app.world_mut().spawn_empty().id();
        let before = serialize(app.world_mut()).expect("serialize");
        app.insert_resource(PanelState);
        app.world_mut().remove_resource::<PanelState>();
        let after = serialize(app.world_mut()).expect("serialize");
        assert_eq!(after, before, "resource storage is not scene content");
        assert!(
            before["ron"]
                .as_str()
                .expect("ron")
                .contains(&format!("// entity {} ", empty.to_bits()))
        );
        app.world_mut().despawn(empty);
        let after = serialize(app.world_mut()).expect("serialize");
        assert_eq!(
            after["entities"].as_u64(),
            before["entities"].as_u64().map(|n| n - 1)
        );
    }

    #[test]
    fn smoke_serialize_keeps_reflected_resource_changes_observable() {
        let mut app = app();
        app.register_type::<regia_scene::layers::LevelLayers>();
        let before = serialize(app.world_mut()).expect("serialize");
        app.insert_resource(regia_scene::layers::LevelLayers::default());
        let bound = serialize(app.world_mut()).expect("serialize");
        assert!(
            bound["ron"]
                .as_str()
                .expect("ron")
                .contains("regia_scene::layers::LevelLayers")
        );
        assert_ne!(
            bound, before,
            "binding changes must remain visible to the smoke undo check"
        );
        app.world_mut()
            .remove_resource::<regia_scene::layers::LevelLayers>();
        assert_eq!(serialize(app.world_mut()).expect("serialize"), before);
    }

    #[test]
    fn serialize_emits_ron() {
        let mut app = app();
        spawn_named(app.world_mut(), "Serial", [1.0; 3]);
        let out = serialize(app.world_mut()).expect("serializes");
        let ron = out["ron"].as_str().expect("ron text");
        assert!(ron.contains("entities: ["), "wrapped in a struct: {ron}");
        assert!(ron.contains("// entity"), "entities are commented");
        assert!(ron.contains("Serial"), "the spawned entity is named: {ron}");
        assert!(ron.contains("translation:"), "components serialized: {ron}");
        assert!(
            out["entities"].as_u64().unwrap_or(0) >= 1,
            "counts entities (framework entities included): {out}"
        );
    }

    #[test]
    fn globs() {
        assert!(glob_match("Primary*", "Primary Window"));
        assert!(glob_match("*Window", "Primary Window"));
        assert!(glob_match("*ary Win*", "Primary Window"));
        assert!(glob_match("Primary Window", "Primary Window"));
        assert!(!glob_match("Primary", "Primary Window"));
        assert!(!glob_match("*Door", "Primary Window"));
    }

    #[test]
    fn invalid_or_dry_run_edits_never_despawn() {
        let mut app = app();
        let world = app.world_mut();
        let target = spawn_named(world, "keep", [0.0; 3]);
        let bits = target.to_bits();
        let bad = json!({ "edits": [{ "op": "despawn", "entity": bits }, { "op": "nope" }] });
        assert!(apply_edits(world, &bad).is_err());
        assert!(
            world.get_entity(target).is_ok(),
            "a rejected transaction must not despawn"
        );
        let dry = json!({ "edits": [{ "op": "despawn", "entity": bits }], "dry_run": true });
        let out = apply_edits(world, &dry).expect("dry run");
        assert_eq!(out["applied"], false);
        assert!(world.get_entity(target).is_ok(), "dry_run must not despawn");
        let real = json!({ "edits": [{ "op": "despawn", "entity": bits }] });
        apply_edits(world, &real).expect("applies");
        assert!(world.get_entity(target).is_err());
    }
}
