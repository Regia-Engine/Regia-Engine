// SPDX-License-Identifier: MPL-2.0
//! Session drivers (spec 41 § 2.8): Rust code compiled into the game that supplies behavior a game does
//! not script itself, such as a bot that walks while the server is observed.
//!
//! The harness names a driver with `REGIA_SESSION_DRIVER`. [`SessionDriverPlugin`] calls `on_launch` at
//! startup and `on_step` every fixed step; the code that loads levels calls [`driver_before_level_load`]
//! and [`driver_after_level_load`] around a load, and the exit path calls [`driver_finish`], whose
//! result becomes the process's exit code.

use std::collections::BTreeMap;

use regia_app::prelude::*;

/// Name of the environment variable the harness sets.
pub const DRIVER_ENV: &str = "REGIA_SESSION_DRIVER";

/// Behavior hooks, in the order they fire.
pub trait SessionDriver: Send + Sync + 'static {
    /// Once, at startup.
    fn on_launch(&mut self, _world: &mut World) {}
    /// Before a level load starts.
    fn before_level_load(&mut self, _world: &mut World) {}
    /// After a level load; the new level is readable.
    fn after_level_load(&mut self, _world: &mut World) {}
    /// Every fixed step.
    fn on_step(&mut self, _world: &mut World) {}
    /// At exit; the returned code becomes the process's result.
    fn finish(&mut self, _world: &mut World, code: i32) -> i32 {
        code
    }
}

/// Factory of a registered driver.
pub type DriverFactory = fn() -> Box<dyn SessionDriver>;

/// Registered drivers by name.
#[derive(Resource, Default)]
pub struct SessionDrivers {
    factories: BTreeMap<String, DriverFactory>,
}

/// The driver this process runs, if any.
#[derive(Resource)]
pub struct ActiveDriver {
    /// Its registered name.
    pub name: String,
    driver: Option<Box<dyn SessionDriver>>,
}

/// The driver name this process was asked for.
#[derive(Resource, Default)]
struct RequestedDriver(Option<String>);

/// Installs the driver plumbing. `driver: None` reads `REGIA_SESSION_DRIVER`.
#[derive(Default)]
pub struct SessionDriverPlugin {
    /// Explicit driver name (tests); `None` reads the environment.
    pub driver: Option<String>,
}

impl Plugin for SessionDriverPlugin {
    fn build(&self, app: &mut App) {
        let name = self.driver.clone().or_else(|| {
            regia_core::brand::env_var(DRIVER_ENV)
                .ok()
                .filter(|n| !n.is_empty())
        });
        app.init_resource::<SessionDrivers>()
            .insert_resource(RequestedDriver(name))
            .add_systems(Startup, driver_launch)
            .add_systems(FixedUpdate, driver_step.run_if(driver_is_active));
    }
}

/// `App` extension to register drivers.
pub trait SessionDriverAppExt {
    /// Registers `T` under `name`.
    fn register_session_driver<T: SessionDriver + Default>(&mut self, name: &str) -> &mut Self;
}

impl SessionDriverAppExt for App {
    fn register_session_driver<T: SessionDriver + Default>(&mut self, name: &str) -> &mut Self {
        self.world_mut()
            .get_resource_or_insert_with(SessionDrivers::default)
            .factories
            .insert(name.to_owned(), || Box::new(T::default()));
        self
    }
}

fn driver_is_active(active: Option<Res<ActiveDriver>>) -> bool {
    active.is_some()
}

fn with_driver(world: &mut World, f: impl FnOnce(&mut dyn SessionDriver, &mut World)) {
    let Some(mut active) = world.remove_resource::<ActiveDriver>() else {
        return;
    };
    if let Some(driver) = active.driver.as_mut() {
        f(driver.as_mut(), world);
    }
    world.insert_resource(active);
}

fn driver_launch(world: &mut World) {
    let Some(name) = world.get_resource::<RequestedDriver>().and_then(|r| r.0.clone()) else {
        return;
    };
    let factory = world
        .get_resource::<SessionDrivers>()
        .and_then(|d| d.factories.get(&name).copied());
    let Some(factory) = factory else {
        tracing::error!(target: "regia_framework", driver = %name, "session driver is not registered");
        return;
    };
    world.insert_resource(ActiveDriver {
        name,
        driver: Some(factory()),
    });
    with_driver(world, |d, w| d.on_launch(w));
}

fn driver_step(world: &mut World) {
    with_driver(world, |d, w| d.on_step(w));
}

/// Call before a level load starts.
pub fn driver_before_level_load(world: &mut World) {
    with_driver(world, |d, w| d.before_level_load(w));
}

/// Call after a level load, once the new level is readable.
pub fn driver_after_level_load(world: &mut World) {
    with_driver(world, |d, w| d.after_level_load(w));
}

/// Call at exit with the code the game would return; returns the code to exit with.
pub fn driver_finish(world: &mut World, code: i32) -> i32 {
    let mut result = code;
    with_driver(world, |d, w| result = d.finish(w, code));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Resource, Default)]
    struct Order(Vec<String>);

    macro_rules! recorder {
        ($name:ident, $tag:literal, $code:literal) => {
            #[derive(Default)]
            struct $name;
            impl SessionDriver for $name {
                fn on_launch(&mut self, w: &mut World) {
                    w.resource_mut::<Order>().0.push(format!("{}:launch", $tag));
                }
                fn before_level_load(&mut self, w: &mut World) {
                    w.resource_mut::<Order>().0.push(format!("{}:before", $tag));
                }
                fn after_level_load(&mut self, w: &mut World) {
                    let level = w.resource::<Order>().0.len();
                    w.resource_mut::<Order>()
                        .0
                        .push(format!("{}:after@{level}", $tag));
                }
                fn on_step(&mut self, w: &mut World) {
                    w.resource_mut::<Order>().0.push(format!("{}:step", $tag));
                }
                fn finish(&mut self, w: &mut World, code: i32) -> i32 {
                    w.resource_mut::<Order>()
                        .0
                        .push(format!("{}:finish({code})", $tag));
                    $code
                }
            }
        };
    }
    recorder!(Walker, "walker", 17);
    recorder!(Watcher, "watcher", 0);

    fn app(driver: &str) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<Order>()
            .register_session_driver::<Walker>("walker")
            .register_session_driver::<Watcher>("watcher")
            .add_plugins(SessionDriverPlugin {
                driver: Some(driver.to_owned()),
            });
        app
    }

    #[test]
    fn hooks_fire_in_order_and_finish_code_becomes_the_result() {
        let mut app = app("walker");
        app.update();
        driver_before_level_load(app.world_mut());
        driver_after_level_load(app.world_mut());
        let code = driver_finish(app.world_mut(), 0);
        let order = app.world().resource::<Order>().0.clone();
        assert_eq!(order[0], "walker:launch");
        let first_before = order.iter().position(|e| e == "walker:before").expect("before");
        let after = order
            .iter()
            .position(|e| e.starts_with("walker:after"))
            .expect("after");
        assert!(first_before < after);
        assert_eq!(order.last().map(String::as_str), Some("walker:finish(0)"));
        assert_eq!(code, 17);
    }

    #[test]
    fn two_instances_use_different_drivers() {
        let mut a = app("walker");
        let mut b = app("watcher");
        a.update();
        b.update();
        let (ca, cb) = (driver_finish(a.world_mut(), 5), driver_finish(b.world_mut(), 5));
        assert_eq!((ca, cb), (17, 0));
        assert!(
            a.world()
                .resource::<Order>()
                .0
                .iter()
                .all(|e| e.starts_with("walker"))
        );
        assert!(
            b.world()
                .resource::<Order>()
                .0
                .iter()
                .all(|e| e.starts_with("watcher"))
        );
    }

    #[test]
    fn without_a_driver_nothing_runs_and_the_code_passes_through() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SessionDriverPlugin { driver: None }));
        app.update();
        assert_eq!(driver_finish(app.world_mut(), 4), 4);
    }
}
