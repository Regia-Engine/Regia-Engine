// SPDX-License-Identifier: MPL-2.0
//! Android packaging support: the generated Gradle project and the toolchain it needs
//! (`docs/specs/21-tooling-and-packaging.md` § 4 Android).

pub mod gradle_project;
pub mod symbols;
pub mod toolchain;

/// Pinned versions of the Java-side build. Bump together and re-verify an APK end to end.
pub mod versions {
    /// Android Gradle plugin.
    pub const AGP_VERSION: &str = "8.13.2";
    /// Gradle used to run the generated project (preferred when several are cached).
    pub const GRADLE_VERSION: &str = "8.14.3";
    /// Oldest Gradle the pinned Android Gradle plugin accepts.
    pub const MIN_GRADLE_VERSION: &str = "8.13";
    /// `androidx.games:games-activity`; must equal the GameActivity version `android-activity`
    /// compiles its C glue against (android-activity 0.6.1 → 4.4.0).
    pub const GAMES_ACTIVITY_VERSION: &str = "4.4.0";
    /// `androidx.appcompat:appcompat` (GameActivity needs an AppCompat theme).
    pub const APPCOMPAT_VERSION: &str = "1.7.0";
    /// `org.jetbrains.kotlin:kotlin-bom`: the kotlin-stdlib version appcompat resolves to; aligns the
    /// legacy `kotlin-stdlib-jdk7/jdk8` artifacts so their classes are not duplicated.
    pub const KOTLIN_VERSION: &str = "1.8.22";
    /// `com.google.android.play:asset-delivery`: the store's asset-pack Java API, linked only when the
    /// project has fast-follow or on-demand chunks (specs/40 § 2.6, ADR-1876).
    pub const ASSET_DELIVERY_VERSION: &str = "2.2.2";
}

/// Native library name GameActivity loads (`lib<name>.so`); the Android crate's `[lib] name`.
pub const NATIVE_LIB_NAME: &str = "main";

/// Simple name of the generated activity class, in the package named by the project id.
pub const ACTIVITY_CLASS: &str = "RegiaActivity";

/// The `am start -n` component of the generated activity.
pub fn launch_component(app_id: &str) -> String {
    format!("{app_id}/.{ACTIVITY_CLASS}")
}
