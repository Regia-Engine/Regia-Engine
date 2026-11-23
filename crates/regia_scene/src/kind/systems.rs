// SPDX-License-Identifier: MPL-2.0
//! Which systems read and write which components, from the schedules' system access metadata
//! (spec 101 § 6.1).
//!
//! [`read_system_access`] walks every built schedule in the world's `Schedules` and records each
//! system's component reads and writes (`SystemAccess::Shared`), or marks it exclusive. A schedule
//! that is running is out of the world while it runs (the agent serves requests inside `PreUpdate`),
//! so [`SystemAccessRecord`] keeps a copy read from `Last` and [`SystemAccessTable::merge`] fills
//! the gaps.

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemAccess;
use regia_ecs::reflect::AppTypeRegistry;
use serde::{Deserialize, Serialize};

use super::type_path_of_component;

/// One system's access.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemAccessEntry {
    /// The system's name (its function path).
    pub name: String,
    /// Component and resource type paths it reads (writes not repeated), sorted.
    pub reads: Vec<String>,
    /// Component and resource type paths it writes, sorted.
    pub writes: Vec<String>,
    /// It takes the whole world (`&mut World`) or reads/writes every component.
    pub exclusive: bool,
}

/// One schedule's systems.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleAccess {
    /// The schedule label (`Update`, `FixedUpdate`).
    pub schedule: String,
    /// Its systems, sorted by name.
    pub systems: Vec<SystemAccessEntry>,
}

/// Every readable schedule's systems.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemAccessTable {
    /// Schedules sorted by label.
    pub schedules: Vec<ScheduleAccess>,
    /// Schedules that could not be read (not built yet), sorted.
    pub unread: Vec<String>,
}

impl SystemAccessTable {
    /// Adds the schedules of `other` this table lacks (a schedule read here wins), and drops from
    /// `unread` every schedule now present.
    pub fn merge(&mut self, other: &SystemAccessTable) {
        for schedule in &other.schedules {
            if !self.schedules.iter().any(|s| s.schedule == schedule.schedule) {
                self.schedules.push(schedule.clone());
            }
        }
        self.schedules.sort_by(|a, b| a.schedule.cmp(&b.schedule));
        for unread in &other.unread {
            if !self.unread.contains(unread) {
                self.unread.push(unread.clone());
            }
        }
        let present: Vec<String> = self.schedules.iter().map(|s| s.schedule.clone()).collect();
        self.unread.retain(|u| !present.contains(u));
        self.unread.sort();
    }

    /// The `(schedule, system)` pairs that read `type_path` (and do not write it).
    #[must_use]
    pub fn readers_of(&self, type_path: &str) -> Vec<(String, String)> {
        self.pairs(|s| s.reads.iter().any(|r| r == type_path))
    }

    /// The `(schedule, system)` pairs that write `type_path`.
    #[must_use]
    pub fn writers_of(&self, type_path: &str) -> Vec<(String, String)> {
        self.pairs(|s| s.writes.iter().any(|w| w == type_path))
    }

    /// The `(schedule, system)` pairs of exclusive systems.
    #[must_use]
    pub fn exclusive(&self) -> Vec<(String, String)> {
        self.pairs(|s| s.exclusive)
    }

    fn pairs(&self, keep: impl Fn(&SystemAccessEntry) -> bool) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .schedules
            .iter()
            .flat_map(|schedule| {
                schedule
                    .systems
                    .iter()
                    .filter(|s| keep(s))
                    .map(|s| (schedule.schedule.clone(), s.name.clone()))
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Reads the access of every built schedule in the world (schedules not built yet go to `unread`;
/// a schedule that is running is absent from the world and not listed at all).
#[must_use]
pub fn read_system_access(world: &World) -> SystemAccessTable {
    let Some(schedules) = world.get_resource::<Schedules>() else {
        return SystemAccessTable::default();
    };
    let registry = world.get_resource::<AppTypeRegistry>().cloned();
    let name_of = |id| {
        type_path_of_component(world, registry.as_ref(), id)
            .unwrap_or_else(|| format!("component#{}", id.index()))
    };
    let mut table = SystemAccessTable::default();
    for (label, schedule) in schedules.iter() {
        let label_name = format!("{label:?}");
        let Ok(systems) = schedule.systems_with_access() else {
            table.unread.push(label_name);
            continue;
        };
        let mut entries: Vec<SystemAccessEntry> = systems
            .enumerate()
            .map(|(index, (_, system))| {
                let mut name = system.system().name().to_string();
                if name.starts_with('<') {
                    name = format!("system#{index}");
                }
                let mut entry = SystemAccessEntry {
                    name,
                    ..SystemAccessEntry::default()
                };
                match system.access() {
                    SystemAccess::None => {}
                    SystemAccess::Exclusive => entry.exclusive = true,
                    SystemAccess::Shared(set) => {
                        let access = set.combined_access();
                        match (access.reads().as_finite_set(), access.writes().as_finite_set()) {
                            (Some(reads), Some(writes)) => {
                                entry.writes = writes.iter().map(name_of).collect();
                                entry.reads = reads
                                    .iter()
                                    .filter(|id| !writes.contains(*id))
                                    .map(name_of)
                                    .collect();
                            }
                            // Unbounded (reads or writes every component): as good as exclusive here.
                            _ => entry.exclusive = true,
                        }
                    }
                }
                entry.reads.sort();
                entry.reads.dedup();
                entry.writes.sort();
                entry.writes.dedup();
                entry
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        table.schedules.push(ScheduleAccess {
            schedule: label_name,
            systems: entries,
        });
    }
    table.schedules.sort_by(|a, b| a.schedule.cmp(&b.schedule));
    table.unread.sort();
    table
}

/// A copy of the system access read from `Last`, where every schedule but `Last` is in the world.
/// Empty until [`SystemAccessRecord::request`] is called; [`record_system_access`] then fills it once.
#[derive(Resource, Clone, Debug, Default)]
pub struct SystemAccessRecord {
    requested: bool,
    table: Option<SystemAccessTable>,
}

impl SystemAccessRecord {
    /// Asks for a record on the next `Last`.
    pub fn request(&mut self) {
        self.requested = true;
    }

    /// The record, once taken.
    #[must_use]
    pub fn table(&self) -> Option<&SystemAccessTable> {
        self.table.as_ref()
    }
}

/// Run condition of [`record_system_access`]: a record was asked for and not taken yet.
#[must_use]
pub fn record_pending(record: Option<Res<SystemAccessRecord>>) -> bool {
    record.is_some_and(|r| r.requested && r.table.is_none())
}

/// Takes the record (an exclusive system for `Last`, gated by [`record_pending`]).
pub fn record_system_access(world: &mut World) {
    let _span = tracing::info_span!("record_system_access").entered();
    let table = read_system_access(world);
    if let Some(mut record) = world.get_resource_mut::<SystemAccessRecord>() {
        record.table = Some(table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component, Default)]
    struct Health(u32);

    #[derive(Component, Default)]
    struct Armor;

    fn heal(mut q: Query<&mut Health, With<Armor>>) {
        for mut h in &mut q {
            h.0 += 1;
        }
    }

    fn look(q: Query<&Health>) {
        let _ = q.iter().count();
    }

    fn everything(_world: &mut World) {}

    #[test]
    fn reads_writes_and_exclusive_systems_are_recorded() {
        let mut app = App::new();
        app.add_systems(Update, (heal, look, everything));
        app.update();
        let table = read_system_access(app.world());
        let health = std::any::type_name::<Health>();
        let writers = table.writers_of(health);
        assert_eq!(writers.len(), 1, "{table:?}");
        assert!(writers[0].1.ends_with("heal"), "{writers:?}");
        let readers = table.readers_of(health);
        assert!(readers.iter().any(|(_, s)| s.ends_with("look")), "{readers:?}");
        assert!(
            !readers.iter().any(|(_, s)| s.ends_with("heal")),
            "a writer is not also a reader"
        );
        assert!(table.exclusive().iter().any(|(_, s)| s.ends_with("everything")));
        // Two reads of the same world give the same table.
        assert_eq!(read_system_access(app.world()), table);
    }

    #[test]
    fn merge_fills_missing_schedules_and_clears_unread() {
        let mut a = SystemAccessTable {
            schedules: vec![ScheduleAccess {
                schedule: "Update".into(),
                systems: Vec::new(),
            }],
            unread: vec!["Last".into()],
        };
        let b = SystemAccessTable {
            schedules: vec![
                ScheduleAccess {
                    schedule: "Last".into(),
                    systems: Vec::new(),
                },
                ScheduleAccess {
                    schedule: "Update".into(),
                    systems: vec![SystemAccessEntry::default()],
                },
            ],
            unread: Vec::new(),
        };
        a.merge(&b);
        let names: Vec<&str> = a.schedules.iter().map(|s| s.schedule.as_str()).collect();
        assert_eq!(names, ["Last", "Update"]);
        assert!(a.schedules[1].systems.is_empty(), "the schedule read here wins");
        assert!(a.unread.is_empty());
    }

    #[test]
    fn the_record_is_taken_once_after_a_request() {
        let mut app = App::new();
        app.init_resource::<SystemAccessRecord>();
        app.add_systems(Last, record_system_access.run_if(record_pending));
        app.add_systems(Update, look);
        app.update();
        assert!(
            app.world().resource::<SystemAccessRecord>().table().is_none(),
            "nothing asked"
        );
        app.world_mut().resource_mut::<SystemAccessRecord>().request();
        app.update();
        let record = app.world().resource::<SystemAccessRecord>();
        let table = record.table().expect("taken");
        assert!(table.schedules.iter().any(|s| s.schedule == "Update"));
        assert!(
            !table.schedules.iter().any(|s| s.schedule == "Last"),
            "Last runs while it reads"
        );
    }
}
