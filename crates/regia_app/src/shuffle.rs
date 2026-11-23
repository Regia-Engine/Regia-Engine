// SPDX-License-Identifier: MPL-2.0
//! Schedule-order testing: shuffled system order and the ambiguity report
//! (`book/agent-guide/schedule-shuffle.md`).
//!
//! A schedule's systems run in a topological order of their `.before`/`.after`/`.chain()` edges.
//! Systems with no path between them run in an arbitrary order. The shipping executor is multi-threaded, so they may run
//! concurrently; the seed tool installs a single-threaded executor so that one seed gives one
//! repeatable order. Code that only works in that order is a latent bug. Two tools:
//!
//! - [`App::shuffle_schedules`] re-sorts every schedule's unconstrained systems from a seed. A scene
//!   whose observable state differs between seeds has a real missing edge.
//! - [`App::ambiguity_report`] lists the system pairs that touch the same component or resource
//!   (one of them writing) with no order between them, ranked, per schedule.
//!
//! Both must run after the plugins are added and before the first [`App::update`]: a schedule is
//! built on its first run and keeps its order afterwards.

use std::fmt::Write as _;

use regia_ecs::ecs::system::SystemAccess;
use regia_ecs::prelude::*;

#[cfg(feature = "debug_names")]
use regia_ecs::ecs::schedule::SingleThreadedExecutor;

use crate::App;

#[cfg(feature = "debug_names")]
impl App {
    /// Randomizes the order of unconstrained systems in every schedule from `seed`, and runs the
    /// schedules on the single-threaded executor so the same seed gives the same order on every run.
    ///
    /// Explicit ordering (`.before`, `.after`, `.chain()`, sets configured with them) is kept; only
    /// systems with no path between them move. Call it once, after adding plugins and before the
    /// first [`App::update`]. A schedule that has already run keeps its order.
    ///
    /// Only available with the `debug_names` feature (on by default): bevy's shuffle setting lives
    /// behind it.
    pub fn shuffle_schedules(&mut self, seed: u64) -> &mut Self {
        let mut schedules = self.world_mut().resource_mut::<Schedules>();
        for (_, schedule) in schedules.iter_mut() {
            let mut settings = schedule.get_build_settings();
            settings.shuffle_seed = Some(seed);
            schedule.set_build_settings(settings);
            schedule.set_executor(SingleThreadedExecutor::new());
        }
        self
    }
}

impl App {
    /// Builds every schedule (without running a system) and lists the system pairs that conflict
    /// on data and have no order between them, per schedule.
    ///
    /// Call it after adding plugins; it does not change the order systems run in. A schedule that
    /// fails to build is listed in [`AmbiguityReport::build_errors`].
    pub fn ambiguity_report(&mut self) -> AmbiguityReport {
        let labels: Vec<_> = self
            .world()
            .resource::<Schedules>()
            .iter()
            .map(|(_, schedule)| schedule.label())
            .collect();
        let mut report = AmbiguityReport::default();
        for label in labels {
            let built = self
                .world_mut()
                .schedule_scope(label, |world, schedule| schedule.initialize(world).map(|_| ()));
            let name = format!("{label:?}");
            if let Err(error) = built {
                report.build_errors.push(format!("{name}: {error:?}"));
                continue;
            }
            let world = self.world();
            let Some(schedule) = world.resource::<Schedules>().get(label) else {
                continue;
            };
            // A built schedule has moved its systems out of the graph into the executable.
            let systems: std::collections::HashMap<_, _> =
                schedule.systems_with_access().into_iter().flatten().collect();
            let name_of = |key| {
                systems.get(&key).map_or_else(
                    || format!("{key:?}"),
                    |s| s.system().name().shortname().to_string(),
                )
            };
            let mut pairs = Vec::new();
            for (a, b, ids) in schedule.graph().conflicting_systems().iter() {
                let writes = |key, id| match systems.get(&key).map(|s| s.access()) {
                    Some(SystemAccess::Shared(set)) => set.combined_access().writes().contains(id),
                    _ => false,
                };
                let write_write = ids
                    .iter()
                    .filter(|id| writes(*a, **id) && writes(*b, **id))
                    .count();
                let mut conflicts: Vec<String> = ids
                    .iter()
                    .map(|id| {
                        world
                            .components()
                            .get_name(*id)
                            .map_or_else(|| format!("{id:?}"), |n| n.shortname().to_string())
                    })
                    .collect();
                conflicts.sort();
                pairs.push(AmbiguousPair {
                    a: name_of(*a),
                    b: name_of(*b),
                    exclusive: ids.is_empty(),
                    write_write,
                    conflicts,
                });
            }
            report.schedules.push(ScheduleAmbiguities {
                schedule: name,
                systems: schedule.systems_len(),
                pairs,
            });
        }
        report.schedules.sort_by(|x, y| {
            y.pairs
                .len()
                .cmp(&x.pairs.len())
                .then(x.schedule.cmp(&y.schedule))
        });
        report
    }
}

/// Two systems with conflicting access and no order between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousPair {
    /// The first system's name.
    pub a: String,
    /// The second system's name.
    pub b: String,
    /// True when one of them takes the whole world (`&mut World`, `Query<EntityMut>`): no single
    /// component to name.
    pub exclusive: bool,
    /// How many of the conflicting components or resources both systems write.
    pub write_write: usize,
    /// The conflicting component and resource names, sorted.
    pub conflicts: Vec<String>,
}

impl AmbiguousPair {
    /// Higher is more likely to matter: shared writes first, then read/write conflicts, then
    /// whole-world systems (which conflict with everything and say little).
    pub fn score(&self) -> usize {
        if self.exclusive {
            1
        } else {
            3 * self.write_write + (self.conflicts.len() - self.write_write)
        }
    }
}

/// The ambiguities of one schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleAmbiguities {
    /// The schedule label's debug name.
    pub schedule: String,
    /// How many systems the schedule holds.
    pub systems: usize,
    /// The conflicting unordered pairs.
    pub pairs: Vec<AmbiguousPair>,
}

/// What [`App::ambiguity_report`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmbiguityReport {
    /// One entry per schedule, the most ambiguous first.
    pub schedules: Vec<ScheduleAmbiguities>,
    /// Schedules that failed to build (a cycle, for instance), with the error.
    pub build_errors: Vec<String>,
}

impl AmbiguityReport {
    /// Total number of ambiguous pairs.
    pub fn total(&self) -> usize {
        self.schedules.iter().map(|s| s.pairs.len()).sum()
    }

    /// Every pair as `(schedule, pair)`, highest [`AmbiguousPair::score`] first (ties keep the
    /// schedule order, then the report order).
    pub fn ranked(&self) -> Vec<(&str, &AmbiguousPair)> {
        let mut all: Vec<_> = self
            .schedules
            .iter()
            .flat_map(|s| s.pairs.iter().map(move |p| (s.schedule.as_str(), p)))
            .collect();
        all.sort_by_key(|(_, p)| std::cmp::Reverse(p.score()));
        all
    }

    /// A markdown summary: counts per schedule, then the `limit` highest-ranked pairs.
    pub fn to_markdown(&self, limit: usize) -> String {
        let mut out = String::from("| schedule | systems | ambiguous pairs |\n|---|---|---|\n");
        for s in &self.schedules {
            let _ = writeln!(out, "| {} | {} | {} |", s.schedule, s.systems, s.pairs.len());
        }
        let _ = writeln!(
            out,
            "\nTop {limit} by score (write/write 3, read/write 1, whole-world 1):\n"
        );
        for (schedule, pair) in self.ranked().into_iter().take(limit) {
            let on = if pair.exclusive {
                "whole world".to_string()
            } else {
                pair.conflicts.join(", ")
            };
            let _ = writeln!(
                out,
                "- `{schedule}` score {}: {} <-> {} on {on}",
                pair.score(),
                pair.a,
                pair.b
            );
        }
        for error in &self.build_errors {
            let _ = writeln!(out, "- build error: {error}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Update;

    #[derive(Resource, Default)]
    struct Log(Vec<&'static str>);

    #[derive(Resource, Default)]
    struct Other(u32);

    fn push(name: &'static str) -> impl FnMut(ResMut<Log>) {
        move |mut log: ResMut<Log>| log.0.push(name)
    }

    fn six_writers() -> App {
        let mut app = App::new();
        app.init_resource::<Log>();
        for name in ["a", "b", "c", "d", "e", "f"] {
            app.add_systems(Update, push(name));
        }
        app
    }

    #[test]
    fn unordered_writers_are_reported_and_a_chain_removes_the_report() {
        let mut app = six_writers();
        let report = app.ambiguity_report();
        let update = report
            .schedules
            .iter()
            .find(|s| s.schedule == "Update")
            .expect("Update is built");
        // Six writers of one resource: every pair conflicts.
        assert_eq!(update.pairs.len(), 15, "{report:?}");
        assert!(update.pairs.iter().all(|p| p.write_write == 1 && !p.exclusive));
        assert!(
            update.pairs[0].conflicts[0].contains("Log"),
            "{:?}",
            update.pairs[0]
        );
        assert!(report.to_markdown(3).contains("score 3"));

        let mut chained = App::new();
        chained.init_resource::<Log>();
        chained.add_systems(Update, (push("a"), push("b"), push("c")).chain());
        assert_eq!(chained.ambiguity_report().total(), 0);
    }

    #[test]
    fn a_read_and_a_write_rank_below_two_writes() {
        let mut app = App::new();
        app.init_resource::<Log>().init_resource::<Other>();
        app.add_systems(Update, push("w"));
        app.add_systems(Update, |_: Res<Log>| {});
        app.add_systems(Update, |mut o: ResMut<Other>| o.0 += 1);
        app.add_systems(Update, |mut o: ResMut<Other>| o.0 += 1);
        let ranked_scores: Vec<usize> = app
            .ambiguity_report()
            .ranked()
            .iter()
            .map(|(_, p)| p.score())
            .collect();
        assert_eq!(ranked_scores, [3, 1]);
    }

    #[cfg(feature = "debug_names")]
    #[test]
    fn seeds_change_the_order_of_unordered_systems_and_never_of_a_chain() {
        let order = |seed: Option<u64>, chained: bool| {
            let mut app = App::new();
            app.init_resource::<Log>();
            if chained {
                app.add_systems(
                    Update,
                    (push("a"), push("b"), push("c"), push("d"), push("e"), push("f")).chain(),
                );
            } else {
                for name in ["a", "b", "c", "d", "e", "f"] {
                    app.add_systems(Update, push(name));
                }
            }
            if let Some(seed) = seed {
                app.shuffle_schedules(seed);
            }
            app.update();
            app.world().resource::<Log>().0.clone()
        };
        let orders: std::collections::BTreeSet<_> = (0..16).map(|s| order(Some(s), false)).collect();
        assert!(orders.len() > 1, "16 seeds gave one order: {orders:?}");
        assert_eq!(
            order(Some(5), false),
            order(Some(5), false),
            "a seed is repeatable"
        );
        let chained: std::collections::BTreeSet<_> = (0..16).map(|s| order(Some(s), true)).collect();
        assert_eq!(chained.len(), 1, "a chain moved: {chained:?}");
        assert_eq!(
            chained.into_iter().next(),
            Some(vec!["a", "b", "c", "d", "e", "f"])
        );
    }

    #[test]
    fn building_the_report_does_not_stop_the_app_from_running() {
        let mut app = six_writers();
        let _ = app.ambiguity_report();
        app.update();
        assert_eq!(app.world().resource::<Log>().0.len(), 6);
    }
}
