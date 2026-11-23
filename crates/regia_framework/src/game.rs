// SPDX-License-Identifier: MPL-2.0
//! Game plugin entry (`docs/specs/14-scene-and-framework.md` § B1, `docs/specs/20-hot-reload.md` § 2).
//!
//! A Regia Engine game project ships its code as a [`GamePlugin`]: a unit of registration (types,
//! scripts, systems, assets) that the launcher, the Android shell and the hot-reloading editor all
//! load the same way. `export_game!` generates the two dylib symbols the editor resolves
//! (`regia_game_entry` and `REGIA_ABI_HASH`).
//!
//! # Hot reload
//!
//! State that should survive a reload lives in reflected components, resources and scripts
//! (`specs/20` § 4). [`GamePlugin::on_unload`] runs *before* the snapshot: join threads the plugin
//! spawned and write their state into reflected resources. [`GamePlugin::on_load`] runs *after* the
//! restore: restart those threads from the restored state.

use regia_app::prelude::*;

/// A game's registration and lifecycle unit (§ B1).
///
/// Implement this for the game's root type and pass it to `export_game!`. To run the same plugin
/// in a plain launcher or test app, use [`GamePluginEntry`], which adapts any `GamePlugin` to an
/// app [`Plugin`] by delegating to [`GamePlugin::register`].
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_framework::game::{GamePlugin, GamePluginEntry};
///
/// #[derive(Resource, Default)]
/// struct Marker;
///
/// struct MyGame;
///
/// impl GamePlugin for MyGame {
///     fn register(&self, app: &mut App) {
///         app.init_resource::<Marker>();
///     }
/// }
///
/// // Documented hot-reload form (`book/user-guide/hot-reload.md`).
/// regia_framework::export_game!(MyGame);
///
/// // `GamePluginEntry` adapts the game to an app `Plugin` for a plain launcher/test app.
/// let mut app = App::new();
/// app.add_plugins((MinimalPlugins, GamePluginEntry(MyGame)));
/// assert!(app.world().get_resource::<Marker>().is_some());
/// ```
pub trait GamePlugin: Send + Sync + 'static {
    /// Registers types, scripts, systems, rules and assets. Called once when the game library is
    /// loaded (and again, on a fresh world state, after every hot reload).
    fn register(&self, app: &mut App);

    /// Joins game-spawned threads and writes their state into reflected resources. Runs **before**
    /// the hot-reload snapshot, so that state is saved (`specs/20` § 3 step 4).
    fn on_unload(&self, _world: &mut World) {}

    /// Restarts those threads from the restored resources. Runs after the snapshot restore
    /// (`specs/20` § 3 step 10).
    fn on_load(&self, _world: &mut World) {}
}

/// Runs any [`GamePlugin`] as an ordinary app [`Plugin`] (launchers, Android shells, tests).
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_framework::game::{GamePlugin, GamePluginEntry};
///
/// #[derive(Resource, Default)]
/// struct Marker;
///
/// struct MyGame;
///
/// impl GamePlugin for MyGame {
///     fn register(&self, app: &mut App) {
///         app.init_resource::<Marker>();
///     }
/// }
///
/// let mut app = App::new();
/// app.add_plugins(GamePluginEntry(MyGame));
/// assert!(app.world().get_resource::<Marker>().is_some());
/// ```
pub struct GamePluginEntry<G: GamePlugin>(pub G);

impl<G: GamePlugin> Plugin for GamePluginEntry<G> {
    fn build(&self, app: &mut App) {
        self.0.register(app);
    }

    fn name(&self) -> &str {
        std::any::type_name::<G>()
    }
}

/// The export table a game library hands the hot-reload host (`specs/20` § 2): three function
/// pointers to the game's [`GamePlugin`] calls. [`export_game!`](crate::export_game) generates it
/// and the `regia_game_entry` symbol that returns its address; the editor copies it out of the
/// loaded library.
///
/// Field order and `#[repr(C)]` are the ABI between editor and game builds. Do not reorder.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GameExports {
    /// [`GamePlugin::register`].
    pub register: extern "Rust" fn(&mut App),
    /// [`GamePlugin::on_unload`].
    pub on_unload: extern "Rust" fn(&mut World),
    /// [`GamePlugin::on_load`].
    pub on_load: extern "Rust" fn(&mut World),
}

/// Paths the [`export_game!`](crate::export_game) expansion names in the game crate.
#[doc(hidden)]
pub mod __export {
    pub use regia_app::prelude::{App, World};
}

/// The ABI hash a game library exports in `REGIA_ABI_HASH` (`specs/20` § 2).
///
/// The real value is computed by the editor's `build.rs` (rustc version, package version, target
/// and profile) and passed to the game build as `REGIA_ABI_HASH=<u64>`; the host compares it and
/// refuses a mismatch ("rebuild the editor"). This function is only the fallback for a game built
/// without that pin (plain `cargo build`): the FNV-1a 64 hash of the framework's version. Such a
/// library never equals the editor's value, so the editor refuses it; the fallback does not
/// detect a rustc, target or feature difference by itself (ADR-2237).
///
/// `const` so [`export_game!`](crate::export_game) can initialise the exported
/// `REGIA_ABI_HASH` static with it (a static's initialiser cannot call a runtime function).
pub const fn abi_hash() -> u64 {
    abi_hash_from(option_env!("REGIA_ABI_HASH"))
}

/// [`abi_hash`] with the pin read by the caller: [`export_game!`](crate::export_game) passes
/// `option_env!("REGIA_ABI_HASH")` expanded in the *game* crate, so the editor's per-build pin
/// rebuilds only the game library, never the engine (`specs/20` § 1 feature anchor).
pub const fn abi_hash_from(pinned: Option<&str>) -> u64 {
    if let Some(pinned) = pinned
        && let Some(value) = parse_decimal_u64(pinned)
    {
        return value;
    }
    regia_core::id::TypePathHash::of(concat!("regia:", env!("CARGO_PKG_VERSION"))).get()
}

/// `const` decimal parser for the `REGIA_ABI_HASH` pin (`str::parse` is not `const`).
///
/// Returns `None` for non-digits or values above `u64::MAX`, in which case
/// [`abi_hash`] falls back to the computed hash (same behaviour as the old
/// runtime `parse().ok()` path).
const fn parse_decimal_u64(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut value: u64 = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte < b'0' || byte > b'9' {
            return None;
        }
        let digit = (byte - b'0') as u64;
        // `checked_*` returns Option, whose combinators are not const; use the overflowing pair.
        let (scaled, overflow_mul) = value.overflowing_mul(10);
        let (next, overflow_add) = scaled.overflowing_add(digit);
        if overflow_mul || overflow_add {
            return None;
        }
        value = next;
        index += 1;
    }
    if bytes.is_empty() { None } else { Some(value) }
}

/// Generates the two hot-reload symbols of a game library (§ B1, `specs/20` § 2):
///
/// - `regia_game_entry() -> *const GameExports` — the address of a static [`GameExports`]
///   table whose three calls forward to the game's [`GamePlugin`] (built once, on first use);
/// - `REGIA_ABI_HASH: u64` — [`abi_hash_from`] of the `REGIA_ABI_HASH` the editor set for
///   this build.
///
/// This replaces a hand-written table: the editor's hot-reload host loads exactly this layout.
/// `#[unsafe(no_mangle)]` needs `unsafe_code`; the macro allows it on exactly these two items so
/// the calling crate keeps the workspace's `unsafe_code = "deny"` (same pattern as
/// `regia::android_main!`, ADR-012).
///
/// # Example
///
/// ```
/// # use regia_framework::GamePlugin;
/// # use regia_app::prelude::*;
/// # struct MyGame;
/// # impl GamePlugin for MyGame {
/// #     fn register(&self, _app: &mut App) {}
/// # }
/// regia_framework::export_game!(MyGame);
/// ```
#[macro_export]
macro_rules! export_game {
    ($game:expr) => {
        #[doc(hidden)]
        fn __regia_game() -> &'static dyn $crate::GamePlugin {
            static GAME: ::std::sync::OnceLock<::std::boxed::Box<dyn $crate::GamePlugin>> =
                ::std::sync::OnceLock::new();
            GAME.get_or_init(|| ::std::boxed::Box::new($game)).as_ref()
        }

        #[doc(hidden)]
        extern "Rust" fn __regia_register(app: &mut $crate::game::__export::App) {
            __regia_game().register(app);
        }

        #[doc(hidden)]
        extern "Rust" fn __regia_on_unload(world: &mut $crate::game::__export::World) {
            __regia_game().on_unload(world);
        }

        #[doc(hidden)]
        extern "Rust" fn __regia_on_load(world: &mut $crate::game::__export::World) {
            __regia_game().on_load(world);
        }

        #[doc(hidden)]
        static __REGIA_GAME_EXPORTS: $crate::game::GameExports = $crate::game::GameExports {
            register: __regia_register,
            on_unload: __regia_on_unload,
            on_load: __regia_on_load,
        };

        /// Hot-reload entry (`specs/20` § 2): the address of this library's export table.
        #[allow(unsafe_code)]
        // SAFETY: the name is the host's lookup key (`specs/20` § 2), so it stays unmangled. It belongs
        // to no system library, and a second definition in one library is a link error.
        #[unsafe(no_mangle)]
        pub extern "Rust" fn regia_game_entry() -> *const $crate::game::GameExports {
            &__REGIA_GAME_EXPORTS
        }

        /// Hot-reload ABI hash (`specs/20` § 2); the host refuses a mismatch.
        #[allow(unsafe_code)]
        // SAFETY: as for `regia_game_entry`; the host reads it as a `u64`, which it is.
        #[unsafe(no_mangle)]
        #[used]
        pub static REGIA_ABI_HASH: u64 = $crate::game::abi_hash_from(::core::option_env!("REGIA_ABI_HASH"));
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestGame;

    impl GamePlugin for TestGame {
        fn register(&self, _app: &mut App) {}
    }

    #[test]
    fn game_plugin_entry_registers_through_the_adapter() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, GamePluginEntry(TestGame)));
        app.update();
    }

    #[test]
    fn abi_hash_is_stable_and_env_pinnable() {
        // Not pinned in this build (no REGIA_ABI_HASH in CI's test env): the value must at least
        // be stable across calls and non-zero.
        let a = abi_hash();
        assert_ne!(a, 0, "FNV over a version string never hashes to 0");
        assert_eq!(a, abi_hash());
    }

    #[test]
    fn decimal_pins_parse_like_the_runtime_path() {
        assert_eq!(parse_decimal_u64("0"), Some(0));
        assert_eq!(parse_decimal_u64("42"), Some(42));
        assert_eq!(
            parse_decimal_u64("18446744073709551615"),
            Some(u64::MAX),
            "u64::MAX itself is in range"
        );
        assert_eq!(
            parse_decimal_u64("18446744073709551616"),
            None,
            "one above u64::MAX overflows"
        );
        assert_eq!(parse_decimal_u64(""), None, "empty pin is not a number");
        assert_eq!(parse_decimal_u64("12x4"), None, "non-digits are rejected");
        assert_eq!(parse_decimal_u64("-1"), None, "signs are rejected");
    }

    /// The regression test for the documented form (`book/user-guide/hot-reload.md`): the macro
    /// must compile as written, including the `REGIA_ABI_HASH` static — its initialiser calls
    /// `abi_hash()`, which only works because that function is `const fn`.
    mod export_game_compiles {
        use super::super::{GamePlugin, abi_hash};
        use regia_app::prelude::*;

        struct MacroGame;

        #[derive(Resource, Default)]
        struct Registered;

        impl GamePlugin for MacroGame {
            fn register(&self, app: &mut App) {
                app.init_resource::<Registered>();
            }
        }

        // One expansion per crate: the two `#[no_mangle]` items are process-wide symbols.
        crate::export_game!(MacroGame);

        /// The table exports the current hash and forwards to the game's own `GamePlugin` impl.
        #[test]
        fn the_macro_exports_the_current_abi_hash_and_forwards() {
            assert_eq!(REGIA_ABI_HASH, abi_hash());
            assert!(!regia_game_entry().is_null());
            let mut app = App::new();
            (__REGIA_GAME_EXPORTS.register)(&mut app);
            assert!(app.world().contains_resource::<Registered>());
            (__REGIA_GAME_EXPORTS.on_unload)(app.world_mut());
            (__REGIA_GAME_EXPORTS.on_load)(app.world_mut());
        }
    }
}
