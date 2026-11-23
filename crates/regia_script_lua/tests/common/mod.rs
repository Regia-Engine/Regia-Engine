// SPDX-License-Identifier: MPL-2.0
//! A small world and mod builders shared by the save and reload tests.

use std::collections::BTreeMap;

use regia_core::math::Vec3;
use regia_script_lua::{LuaLimits, LuaMods, ModFiles, ModWorld, ScriptValue};

/// One mod with `main` as its entry chunk and `version` as its version string.
pub fn mod_files(id: &str, version: &str, main: &str) -> ModFiles {
    ModFiles::from_map([
        ("luamod.ron".to_owned(), manifest(id, version)),
        ("main.lua".to_owned(), main.to_owned()),
    ])
}

pub fn manifest(id: &str, version: &str) -> String {
    format!(
        r#"(format_version: 1, id: "{id}", name: "{id}", version: "{version}", entry: "main.lua", api_version: 1)"#
    )
}

pub fn host() -> LuaMods {
    LuaMods::new(1234, LuaLimits::default())
}

/// An entity of [`TestWorld`].
#[derive(Debug, Clone, PartialEq)]
pub struct TestEntity {
    pub name: String,
    pub position: Vec3,
    pub owner: Option<String>,
    /// The script component it carries, like the ECS's single `LuaComponent` slot:
    /// `(declaration name, values)`.
    pub component: Option<(String, Vec<(String, ScriptValue)>)>,
}

/// A `ModWorld` that tracks owners. Ids only grow, so a despawned id is never alive again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TestWorld {
    pub entities: BTreeMap<u64, TestEntity>,
    next: u64,
    /// Properties written through `set_component_props` (one per field of the map) or
    /// `set_component_prop` (one).
    pub field_writes: u64,
    /// Times `owned` listed this mod's entities. `owned_count` does not add to this.
    pub owned_calls: u64,
}

impl TestWorld {
    /// Adds an entity no mod owns.
    pub fn add(&mut self, name: &str, position: Vec3) -> u64 {
        self.next += 1;
        self.entities.insert(
            self.next,
            TestEntity {
                name: name.to_owned(),
                position,
                owner: None,
                component: None,
            },
        );
        self.next
    }

    /// The names of every entity, sorted. (Tests that do not list entities omit this.)
    #[allow(dead_code)]
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.entities.values().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// The position of the first entity called `name`.
    pub fn position_of(&self, name: &str) -> Option<Vec3> {
        self.entities
            .values()
            .find(|e| e.name == name)
            .map(|e| e.position)
    }
}

impl ModWorld for TestWorld {
    fn find(&mut self, name: &str) -> Option<u64> {
        self.entities
            .iter()
            .find(|(_, e)| e.name == name)
            .map(|(id, _)| *id)
    }
    fn alive(&mut self, entity: u64) -> bool {
        self.entities.contains_key(&entity)
    }
    fn name(&mut self, entity: u64) -> Option<String> {
        self.entities.get(&entity).map(|e| e.name.clone())
    }
    fn position(&mut self, entity: u64) -> Option<Vec3> {
        self.entities.get(&entity).map(|e| e.position)
    }
    fn set_position(&mut self, entity: u64, position: Vec3) -> bool {
        match self.entities.get_mut(&entity) {
            Some(e) => {
                e.position = position;
                true
            }
            None => false,
        }
    }
    fn spawn(&mut self, owner: &str, name: &str, position: Vec3) -> Option<u64> {
        let id = self.add(name, position);
        if let Some(e) = self.entities.get_mut(&id) {
            e.owner = Some(owner.to_owned());
        }
        Some(id)
    }
    fn despawn(&mut self, owner: &str, entity: u64) -> bool {
        let owned = self
            .entities
            .get(&entity)
            .is_some_and(|e| e.owner.as_deref() == Some(owner));
        owned && self.entities.remove(&entity).is_some()
    }
    fn owned(&mut self, owner: &str) -> Vec<u64> {
        self.owned_calls = self.owned_calls.saturating_add(1);
        self.entities
            .iter()
            .filter(|(_, e)| e.owner.as_deref() == Some(owner))
            .map(|(id, _)| *id)
            .collect()
    }

    fn owned_count(&mut self, owner: &str) -> usize {
        self.entities
            .values()
            .filter(|e| e.owner.as_deref() == Some(owner))
            .count()
    }

    fn has_component(&mut self, entity: u64, decl: &str) -> bool {
        self.entities
            .get(&entity)
            .is_some_and(|e| e.component.as_ref().is_some_and(|(name, _)| name == decl))
    }

    fn attach_component(&mut self, entity: u64, decl: &str, initial: Vec<(String, ScriptValue)>) -> bool {
        let Some(entry) = self.entities.get_mut(&entity) else {
            return false;
        };
        if entry.component.is_some() {
            return false;
        }
        entry.component = Some((decl.to_owned(), initial));
        true
    }

    fn remove_component(&mut self, entity: u64, decl: &str) -> bool {
        let Some(entry) = self.entities.get_mut(&entity) else {
            return false;
        };
        match &entry.component {
            Some((name, _)) if name == decl => {
                entry.component = None;
                true
            }
            _ => false,
        }
    }

    fn component_props(&mut self, entity: u64, decl: &str) -> Option<Vec<(String, ScriptValue)>> {
        let entry = self.entities.get(&entity)?;
        match &entry.component {
            Some((name, values)) if name == decl => Some(values.clone()),
            _ => None,
        }
    }

    fn set_component_props(&mut self, entity: u64, decl: &str, props: Vec<(String, ScriptValue)>) -> bool {
        let Some(entry) = self.entities.get_mut(&entity) else {
            return false;
        };
        let wrote = match &mut entry.component {
            Some((name, values)) if name == decl => {
                let n = props.len() as u64;
                *values = props;
                Some(n)
            }
            _ => None,
        };
        if let Some(n) = wrote {
            self.field_writes = self.field_writes.saturating_add(n);
            true
        } else {
            false
        }
    }

    fn set_component_prop(&mut self, entity: u64, decl: &str, key: &str, value: ScriptValue) -> bool {
        let Some(entry) = self.entities.get_mut(&entity) else {
            return false;
        };
        let wrote = match &mut entry.component {
            Some((name, values)) if name == decl => {
                match values.iter_mut().find(|(name, _)| name == key) {
                    Some((_, existing)) => *existing = value,
                    None => values.push((key.to_owned(), value)),
                }
                true
            }
            _ => false,
        };
        if wrote {
            self.field_writes = self.field_writes.saturating_add(1);
        }
        wrote
    }

    fn entities_with(&mut self, decl: &str) -> Vec<u64> {
        let mut found: Vec<u64> = self
            .entities
            .iter()
            .filter(|(_, e)| e.component.as_ref().is_some_and(|(name, _)| name == decl))
            .map(|(id, _)| *id)
            .collect();
        found.sort_unstable();
        found
    }
}
