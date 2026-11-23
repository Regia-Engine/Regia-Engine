// SPDX-License-Identifier: MPL-2.0
//! Plugins (`docs/specs/10-core.md` § 3.1).

use crate::App;

/// A unit of app configuration: adds systems, resources and messages.
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
///
/// struct CounterPlugin;
///
/// impl Plugin for CounterPlugin {
///     fn build(&self, app: &mut App) {
///         app.init_resource::<Counter>()
///             .add_systems(Update, count_up);
///     }
/// }
///
/// #[derive(Resource, Default)]
/// struct Counter(u32);
///
/// fn count_up(mut counter: ResMut<Counter>) {
///     counter.0 += 1;
/// }
///
/// let mut app = App::new();
/// app.add_plugins(CounterPlugin);
/// app.update();
/// assert_eq!(app.world().resource::<Counter>().0, 1);
/// ```
pub trait Plugin: Send + Sync + 'static {
    /// Configures the app.
    fn build(&self, app: &mut App);
    /// Runs after every plugin has been built (for example to create GPU resources).
    fn finish(&self, _app: &mut App) {}
    /// Runs after every plugin's `finish`.
    fn cleanup(&self, _app: &mut App) {}
    /// Unique name; a second plugin with the same name is skipped with a warning.
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

/// One plugin or a tuple of plugins, accepted by [`App::add_plugins`].
pub trait Plugins {
    /// Adds the plugin(s) to `app`.
    fn add_to(self, app: &mut App);
}

impl<P: Plugin> Plugins for P {
    fn add_to(self, app: &mut App) {
        app.add_boxed_plugin(Box::new(self));
    }
}

macro_rules! impl_plugins_tuple {
    ($($p:ident),+) => {
        impl<$($p: Plugins),+> Plugins for ($($p,)+) {
            #[allow(non_snake_case)]
            fn add_to(self, app: &mut App) {
                let ($($p,)+) = self;
                $($p.add_to(app);)+
            }
        }
    };
}

impl_plugins_tuple!(A);
impl_plugins_tuple!(A, B);
impl_plugins_tuple!(A, B, C);
impl_plugins_tuple!(A, B, C, D);
impl_plugins_tuple!(A, B, C, D, E);
impl_plugins_tuple!(A, B, C, D, E, F);
impl_plugins_tuple!(A, B, C, D, E, F, G);
impl_plugins_tuple!(A, B, C, D, E, F, G, H);
