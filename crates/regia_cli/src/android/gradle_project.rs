// SPDX-License-Identifier: MPL-2.0
//! Renders the Gradle project under `<project>/build/android/` from the templates in
//! `crates/regia_cli/templates/android/`.
//!
//! Templates use `{{key}}` placeholders. Every value is escaped for the file it lands in (Kotlin string,
//! XML, Android string resource, Java properties), and rendering fails if a placeholder is left over.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use regia_asset::content::{ContentManifest, Delivery, pack_name};

use super::versions::{
    AGP_VERSION, APPCOMPAT_VERSION, ASSET_DELIVERY_VERSION, GAMES_ACTIVITY_VERSION, KOTLIN_VERSION,
};
use super::{ACTIVITY_CLASS, NATIVE_LIB_NAME};
use crate::manifest::{AndroidPlatform, Manifest};

/// Templates rendered verbatim to the same relative path.
const TEMPLATES: &[(&str, &str)] = &[
    (
        "settings.gradle.kts",
        include_str!("../../templates/android/settings.gradle.kts"),
    ),
    (
        "build.gradle.kts",
        include_str!("../../templates/android/build.gradle.kts"),
    ),
    (
        "gradle.properties",
        include_str!("../../templates/android/gradle.properties"),
    ),
    (
        "app/build.gradle.kts",
        include_str!("../../templates/android/app/build.gradle.kts"),
    ),
    (
        "app/src/main/AndroidManifest.xml",
        include_str!("../../templates/android/app/src/main/AndroidManifest.xml"),
    ),
    (
        "app/src/debug/AndroidManifest.xml",
        include_str!("../../templates/android/app/src/debug/AndroidManifest.xml"),
    ),
    (
        "app/src/main/res/values/strings.xml",
        include_str!("../../templates/android/app/src/main/res/values/strings.xml"),
    ),
    (
        "app/src/main/res/values/themes.xml",
        include_str!("../../templates/android/app/src/main/res/values/themes.xml"),
    ),
];

/// The activity source; its path depends on the project id (the Java package).
const ACTIVITY_TEMPLATE: &str = include_str!("../../templates/android/RegiaActivity.java");

/// Where `cargo ndk -o` must put the native libraries, relative to the Gradle project.
pub const JNI_LIBS_DIR: &str = "app/src/main/jniLibs";

/// Generated sources that are deleted before each build so nothing stale (an old ABI, an activity in
/// a previous package) survives. Gradle's own caches (`.gradle/`, `app/build/`) are kept.
pub const GENERATED_SOURCE_DIR: &str = "app/src";

/// Directory (relative to the Gradle project) holding the generated asset-pack modules; wiped and
/// regenerated on every build like [`GENERATED_SOURCE_DIR`].
pub const ASSET_PACKS_DIR: &str = "packs";

/// One store asset pack: a `fast_follow` or `on_demand` chunk (specs/40 § 2.6, ADR-1876).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPackModule {
    /// Pack name (the chunk name with `-` replaced by `_`); also the Gradle module name.
    pub pack: String,
    /// The chunk's pak file name (placed in the module's `src/main/assets/`).
    pub file: String,
    /// `FastFollow` or `OnDemand`.
    pub delivery: Delivery,
}

impl AssetPackModule {
    /// The Gradle `deliveryType` value.
    #[must_use]
    pub fn delivery_type(&self) -> &'static str {
        match self.delivery {
            Delivery::FastFollow => "fast-follow",
            Delivery::OnDemand => "on-demand",
            Delivery::Install | Delivery::External => "install-time",
        }
    }

    /// Where the pak goes, relative to the Gradle project.
    #[must_use]
    pub fn asset_path(&self) -> String {
        format!("{ASSET_PACKS_DIR}/{}/src/main/assets/{}", self.pack, self.file)
    }
}

/// The asset packs of a chunked Android cook: one per `fast_follow` / `on_demand` chunk, in manifest
/// order. `install` chunks stay in the base module's assets and `external` chunks on the static host.
///
/// # Errors
/// A pack name that is also a module of the generated project (`app`) or that two chunks share
/// (`pack_name_collision`; the cook's `check_android_packs` catches the second earlier).
pub fn asset_pack_modules(manifest: &ContentManifest) -> Result<Vec<AssetPackModule>> {
    let mut out: Vec<AssetPackModule> = Vec::new();
    for chunk in &manifest.chunks {
        if !matches!(chunk.delivery, Delivery::FastFollow | Delivery::OnDemand) {
            continue;
        }
        let pack = pack_name(&chunk.name);
        if pack == "app" || out.iter().any(|p| p.pack == pack) {
            bail!(
                "pack_name_collision: chunk {:?} becomes asset pack {pack:?}, which is already a module of the \
                 generated Android project; rename the chunk",
                chunk.name
            );
        }
        out.push(AssetPackModule {
            pack,
            file: chunk.file.clone(),
            delivery: chunk.delivery,
        });
    }
    Ok(out)
}

/// The activity helpers over the store's asset-pack Java API (`regia_platform::asset_packs` calls
/// them over JNI). Rendered only for projects with asset packs, so other builds do not link the
/// library.
const ASSET_PACK_IMPORTS: &str = "import com.google.android.play.core.assetpacks.AssetPackException;
import com.google.android.play.core.assetpacks.AssetPackLocation;
import com.google.android.play.core.assetpacks.AssetPackManager;
import com.google.android.play.core.assetpacks.AssetPackManagerFactory;
import com.google.android.play.core.assetpacks.AssetPackState;
import com.google.android.play.core.assetpacks.model.AssetPackStatus;
import com.google.android.play.core.assetpacks.model.AssetPackStorageMethod;
import java.util.Collections;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
";

const ASSET_PACK_METHODS: &str = "
    // Store asset packs (specs/40 § 2.6): native code polls these; states arrive through a listener.
    private AssetPackManager regiaPacks;
    private final ConcurrentHashMap<String, long[]> regiaPackStates = new ConcurrentHashMap<>();
    // One generation per pack, moved on by every fetch: a callback of an older request (a state
    // query, or the fetch Task a retry cancelled) never overwrites the newer request's state.
    private final ConcurrentHashMap<String, Integer> regiaPackGens = new ConcurrentHashMap<>();
    // Packs cancelled whose CANCELED the store has not reported yet. The store cancels active
    // downloads only, so a pack already in a terminal state is not marked, and any terminal state
    // the listener accepts settles the mark (a download that ends while its cancel is in flight).
    private final Set<String> regiaPackCancelled = ConcurrentHashMap.newKeySet();
    // Packs fetched again before the store reported their cancel, until the listener accepts a
    // row: a CANCELED the listener delivers meanwhile ends the previous download, not this one.
    // A fetch with no cancel pending is not marked, so cancelling it before its first state
    // still lands.
    private final Set<String> regiaPackFresh = ConcurrentHashMap.newKeySet();
    // Removed packs stay gone. A listener row, or a state query already in flight, must not put
    // the pack back. The next fetch clears this.
    private final Set<String> regiaPackGone = ConcurrentHashMap.newKeySet();
    // A cancel this activity requested. Listener rows for that request are ignored until the next
    // fetch, so a late DOWNLOADING cannot put the pack back into progress.
    private final Set<String> regiaPackLocalCancel = ConcurrentHashMap.newKeySet();
    // Guards every read and write of the fields above: the listener (the store's thread) and the
    // store callbacks check a mark or a generation and then write a row, and so do the helpers
    // native code calls on its own thread. Store IPC (fetch, cancel, remove, state queries) runs
    // after the lock is released, so the listener never waits on the store.
    private final Object regiaPackLock = new Object();

    private AssetPackManager regiaPackManager() {
        synchronized (regiaPackLock) {
            if (regiaPacks == null) {
                regiaPacks = AssetPackManagerFactory.getInstance(getApplicationContext());
                regiaPacks.registerListener(this::regiaPackUpdate);
            }
            return regiaPacks;
        }
    }

    /** A state the store leaves on its own: no CANCELED follows a cancel of a pack in one. */
    private static boolean regiaPackTerminal(int status) {
        return status == AssetPackStatus.COMPLETED || status == AssetPackStatus.FAILED
            || status == AssetPackStatus.NOT_INSTALLED || status == AssetPackStatus.CANCELED;
    }

    private void regiaPackUpdate(AssetPackState state) {
        String pack = state.name();
        int status = state.status();
        synchronized (regiaPackLock) {
            if (regiaPackGone.contains(pack) || regiaPackLocalCancel.contains(pack)) {
                return;
            }
            if (status == AssetPackStatus.CANCELED && regiaPackFresh.contains(pack)) {
                return; // The cancelled previous download, reported after the retry started.
            }
            if (regiaPackTerminal(status)) {
                regiaPackCancelled.remove(pack);
            }
            // The first accepted row ends the fresh mark. The old download's CANCELED was
            // already dropped above or ignored under the local-cancel mark, and nothing
            // guarantees the store reports one for the superseded transfer at all: keeping the
            // mark armed until an echo would swallow a real cancel of the retry (a Play
            // download-notification or store-side cancel), and the pack would report a live
            // state forever.
            regiaPackFresh.remove(pack);
            long[] shown = regiaPackStates.get(pack);
            if (shown == null || shown[0] != status) {
                // One line per store status change, never per byte update: a store-track build
                // has no agent, and this is how its check reads the store's own pack states.
                android.util.Log.i(\"regia_pack\", pack + \" status \" + status + \" error \" + state.errorCode()
                    + \" bytes \" + state.bytesDownloaded() + \"/\" + state.totalBytesToDownload());
            }
            regiaPackStates.put(pack, regiaPackRow(state));
        }
    }

    private static long[] regiaPackRow(AssetPackState state) {
        return new long[] {state.status(), state.errorCode(), state.bytesDownloaded(), state.totalBytesToDownload()};
    }

    /**
     * {status, errorCode, bytesDownloaded, totalBytesToDownload}, or null until the store answers
     * (a missing row posts a state query; a later poll reads its answer).
     */
    public long[] regiaPackState(final String pack) {
        AssetPackManager manager = regiaPackManager();
        final int asked;
        synchronized (regiaPackLock) {
            if (regiaPackGone.contains(pack)) {
                return null;
            }
            long[] known = regiaPackStates.get(pack);
            if (known != null) {
                return known;
            }
            asked = regiaPackGens.getOrDefault(pack, 0);
        }
        manager.getPackStates(Collections.singletonList(pack)).addOnSuccessListener(states -> {
            AssetPackState state = states.packStates().get(pack);
            synchronized (regiaPackLock) {
                // A fetch since the query, a removal, or a listener row, is newer than this answer.
                if (state != null && regiaPackGens.getOrDefault(pack, 0) == asked && !regiaPackGone.contains(pack)) {
                    regiaPackStates.putIfAbsent(pack, regiaPackRow(state));
                }
            }
        });
        return null;
    }

    /**
     * Pending from here on, so an old cancelled or failed state is never read after a retry: the
     * older request's callbacks see a newer generation and write nothing.
     */
    public void regiaPackFetch(final String pack) {
        final int gen;
        synchronized (regiaPackLock) {
            regiaPackGone.remove(pack);
            regiaPackLocalCancel.remove(pack);
            gen = regiaPackGens.merge(pack, 1, Integer::sum);
            if (regiaPackCancelled.remove(pack)) {
                regiaPackFresh.add(pack);
            }
            regiaPackStates.put(pack, new long[] {AssetPackStatus.PENDING, 0, 0, 0});
        }
        regiaPackManager().fetch(Collections.singletonList(pack)).addOnFailureListener(error -> {
            int code = error instanceof AssetPackException ? ((AssetPackException) error).getErrorCode() : -1;
            synchronized (regiaPackLock) {
                if (regiaPackGens.getOrDefault(pack, 0) != gen) {
                    return; // A retry superseded (and cancelled) this request.
                }
                regiaPackStates.put(pack, new long[] {AssetPackStatus.FAILED, code, 0, 0});
            }
        });
    }

    /**
     * The fetch's late callbacks are stale from here on; the store's CANCELED is this cancel. A
     * pack whose known state is terminal is not marked and keeps its row: the store reports
     * nothing for it.
     */
    public void regiaPackCancel(String pack) {
        synchronized (regiaPackLock) {
            regiaPackGens.merge(pack, 1, Integer::sum);
            regiaPackFresh.remove(pack);
            long[] row = regiaPackStates.get(pack);
            if (row == null || !regiaPackTerminal((int) row[0])) {
                regiaPackCancelled.add(pack);
                regiaPackLocalCancel.add(pack);
                regiaPackStates.put(pack, new long[] {AssetPackStatus.CANCELED, 0, 0, 0});
            }
        }
        regiaPackManager().cancel(Collections.singletonList(pack));
    }

    /** A state query posted before the removal cannot put the removed pack's row back. */
    public void regiaPackRemove(String pack) {
        synchronized (regiaPackLock) {
            regiaPackGens.merge(pack, 1, Integer::sum);
            regiaPackFresh.remove(pack);
            regiaPackCancelled.remove(pack);
            regiaPackGone.add(pack);
            regiaPackLocalCancel.remove(pack);
            regiaPackStates.remove(pack);
        }
        regiaPackManager().removePack(pack);
    }

    /** The pack's extracted-files directory, or null (not on the device, or kept inside the APK). */
    public String regiaPackLocation(String pack) {
        AssetPackLocation location = regiaPackManager().getPackLocation(pack);
        if (location == null || location.packStorageMethod() != AssetPackStorageMethod.STORAGE_FILES) {
            return null;
        }
        return location.assetsPath();
    }

    /** The store's confirmation dialog (cellular download or other approval), on the UI thread. */
    public void regiaPackConfirm() {
        runOnUiThread(() -> regiaPackManager().showConfirmationDialog(this));
    }
";

/// The `build.gradle.kts` of one asset-pack module.
fn asset_pack_build_file(pack: &AssetPackModule) -> String {
    format!(
        "// Generated by `regia package --platform android` (specs/40 § 2.6). Regenerated on every build; do not edit.\n\
         plugins {{\n    id(\"com.android.asset-pack\")\n}}\n\n\
         assetPack {{\n    packName.set(\"{pack}\")\n    dynamicDelivery {{\n        deliveryType.set(\"{delivery}\")\n    }}\n}}\n",
        pack = pack.pack,
        delivery = pack.delivery_type()
    )
}

/// Toolchain paths written into the generated project.
#[derive(Debug, Clone)]
pub struct Toolchain {
    /// Android SDK root (`local.properties` `sdk.dir`).
    pub sdk_dir: PathBuf,
    /// NDK the Android Gradle plugin uses to strip native debug symbols; without it the libraries
    /// are packaged unstripped.
    pub ndk: Option<Ndk>,
}

/// An installed NDK.
#[derive(Debug, Clone)]
pub struct Ndk {
    /// NDK root directory (`ndkPath`).
    pub dir: PathBuf,
    /// `Pkg.Revision` of the NDK (`ndkVersion`; must agree with `ndkPath` or AGP refuses to strip).
    pub version: Option<String>,
}

/// One file of the generated project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// Path relative to the Gradle project root, `/`-separated.
    pub path: String,
    /// File contents.
    pub contents: String,
}

/// Replaces every `{{key}}` in `template`; fails if an unknown placeholder remains.
pub fn render(template: &str, vars: &[(&str, String)]) -> Result<String> {
    let mut out = template.to_owned();
    for (key, value) in vars {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    if let Some(start) = out.find("{{") {
        let rest = &out[start..];
        let end = rest.find("}}").map_or(rest.len().min(40), |e| e + 2);
        bail!("template placeholder `{}` has no value", &rest[..end]);
    }
    Ok(out)
}

/// Escapes text for an XML attribute value or element content.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes text for the body of an Android `<string>` resource: aapt2's backslash escapes
/// (quotes, apostrophes, backslashes, newlines, a leading `@` or `?`), then XML.
pub fn android_string_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '@' | '?' if i == 0 => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    xml_escape(&out)
}

/// Escapes text for a Kotlin double-quoted string literal.
pub fn kotlin_string_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes text for a Java `.properties` value (`local.properties`).
pub fn properties_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            ':' | '=' | '#' | '!' => {
                out.push('\\');
                out.push(c);
            }
            ' ' if i == 0 => out.push_str("\\ "),
            _ => out.push(c),
        }
    }
    out
}

/// Permissions the engine itself needs on every Android build (normal permissions, granted at install):
/// `VIBRATE` for `regia_platform::Haptics`; `ACCESS_NETWORK_STATE` for the metered-network query of
/// content downloads (`regia_platform::network`, spec 40 § 2.5, ADR-7236).
pub const ENGINE_PERMISSIONS: &[&str] = &[
    "android.permission.VIBRATE",
    "android.permission.ACCESS_NETWORK_STATE",
];

/// The Kotlin expression for a signing password. An `env:VAR` reference becomes
/// `System.getenv("VAR")` so the secret never lands in the generated file under `build/`; a literal
/// (the owner's choice in `Regia.toml`) is quoted.
fn secret_expr(spec: &str) -> String {
    match spec.trim().strip_prefix("env:") {
        Some(var) => format!(
            "(System.getenv(\"{}\") ?: \"\")",
            kotlin_string_escape(var.trim())
        ),
        None => {
            // Committed to `Regia.toml` and copied here under `build/`.
            eprintln!("warning: a signing secret in Regia.toml is a literal; use `env:VAR` (specs/21 § 2)");
            if cfg!(not(any(unix, windows))) {
                eprintln!(
                    "warning: on this host the generated app/build.gradle.kts keeps the folder's permissions, \
                     so other accounts that can read the project can read the secret"
                );
            }
            format!(
                "\"{}\"",
                kotlin_string_escape(&AndroidPlatform::resolve_secret(spec).unwrap_or_default())
            )
        }
    }
}

/// Renders the Gradle `signingConfigs` / `buildTypes` block that signs the release APK/AAB with the
/// project keystore (specs/21 § 4). Returns an empty string when no keystore is configured. AGP 8
/// leaves release builds unsigned in that case (it does not fall back to the debug keystore), so the
/// caller (`package_android`) rejects a `--profile shipping` build without a keystore before Gradle
/// runs. The block is indented to sit inside `android { ... }`.
fn render_signing_config(android: &AndroidPlatform) -> Result<String> {
    let Some(keystore_path) = AndroidPlatform::resolve_secret(&android.keystore) else {
        return Ok(String::new());
    };
    let store_password = secret_expr(&android.keystore_password);
    let key_alias = AndroidPlatform::resolve_secret(&android.key_alias).unwrap_or_default();
    // An explicit key password that does not resolve (unset `env:` variable) is an error: silently
    // falling back to the store password would sign with the wrong credential. A resolving one is
    // written as `secret_expr` (an `env:` reference stays a `System.getenv` call).
    let key_password = match AndroidPlatform::resolve_secret(&android.key_password) {
        Some(_) => secret_expr(&android.key_password),
        None if android.key_password.trim().is_empty() => store_password.clone(),
        None => bail!(
            "[platforms.android] key_password = \"{}\" does not resolve (is the environment variable set?)",
            android.key_password.trim()
        ),
    };

    let mut out = String::new();
    out.push_str("    signingConfigs {\n");
    out.push_str("        create(\"release\") {\n");
    out.push_str(&format!(
        "            storeFile = file(\"{}\")\n",
        kotlin_string_escape(&keystore_path)
    ));
    out.push_str(&format!("            storePassword = {store_password}\n"));
    out.push_str(&format!(
        "            keyAlias = \"{}\"\n",
        kotlin_string_escape(&key_alias)
    ));
    out.push_str(&format!("            keyPassword = {key_password}\n"));
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("    buildTypes {\n");
    out.push_str("        getByName(\"release\") {\n");
    out.push_str("            signingConfig = signingConfigs.getByName(\"release\")\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    Ok(out)
}

/// Renders every file of the Gradle project for `manifest` (pure; nothing touches the disk), plus one
/// asset-pack module per entry of `packs` (`packs/<pack>/build.gradle.kts`), their
/// `settings.gradle.kts` includes, the app's `assetPacks` list, the store's asset-delivery library and
/// the activity's pack helpers. With no packs none of those are written.
pub fn render_project(
    manifest: &Manifest,
    toolchain: &Toolchain,
    packs: &[AssetPackModule],
) -> Result<Vec<GeneratedFile>> {
    let project = &manifest.project;
    let android = &manifest.platforms.android;
    let mut all_permissions: Vec<&str> = ENGINE_PERMISSIONS.to_vec();
    for p in &android.permissions {
        if !all_permissions.contains(&p.as_str()) {
            all_permissions.push(p);
        }
    }
    let permissions: Vec<String> = all_permissions
        .iter()
        .map(|p| format!("    <uses-permission android:name=\"{}\" />", xml_escape(p)))
        .collect();
    let abi_filters: Vec<String> = android
        .abis
        .iter()
        .map(|abi| format!("\"{}\"", kotlin_string_escape(abi)))
        .collect();
    let mut ndk_path = String::new();
    if let Some(ndk) = &toolchain.ndk {
        ndk_path += &format!(
            "    ndkPath = \"{}\"\n",
            kotlin_string_escape(&ndk.dir.to_string_lossy())
        );
        if let Some(version) = &ndk.version {
            ndk_path += &format!("    ndkVersion = \"{}\"\n", kotlin_string_escape(version));
        }
    }
    let signing_config = render_signing_config(android)?;
    let vars = [
        ("agp_version", AGP_VERSION.to_owned()),
        ("games_activity_version", GAMES_ACTIVITY_VERSION.to_owned()),
        ("appcompat_version", APPCOMPAT_VERSION.to_owned()),
        ("kotlin_version", KOTLIN_VERSION.to_owned()),
        // The id is validated as a Java package name, so it needs no escaping in any of the files.
        ("id", project.id.clone()),
        ("app_name", android_string_escape(&project.name)),
        ("version_name", kotlin_string_escape(&project.version)),
        ("version_code", project.build_number.to_string()),
        ("min_sdk", android.min_sdk.to_string()),
        ("target_sdk", android.target_sdk.to_string()),
        ("orientation", android.orientation.android_value().to_owned()),
        ("abi_filters", abi_filters.join(", ")),
        ("permissions", permissions.join("\n")),
        ("ndk_path", ndk_path),
        ("signing_config", signing_config),
        ("lib_name", NATIVE_LIB_NAME.to_owned()),
        (
            "asset_pack_includes",
            packs
                .iter()
                .map(|p| {
                    format!(
                        "include(\":{0}\")\nproject(\":{0}\").projectDir = file(\"{ASSET_PACKS_DIR}/{0}\")\n",
                        p.pack
                    )
                })
                .collect(),
        ),
        (
            "asset_packs",
            if packs.is_empty() {
                String::new()
            } else {
                let list: Vec<String> = packs.iter().map(|p| format!("\":{}\"", p.pack)).collect();
                format!(
                    "\n    // Store asset packs (specs/40 § 2.6): one module per fast-follow / on-demand chunk.\n    assetPacks += listOf({})\n",
                    list.join(", ")
                )
            },
        ),
        (
            "asset_delivery_dependency",
            if packs.is_empty() {
                String::new()
            } else {
                format!(
                    "    implementation(\"com.google.android.play:asset-delivery:{ASSET_DELIVERY_VERSION}\")\n"
                )
            },
        ),
        (
            "asset_pack_imports",
            if packs.is_empty() {
                String::new()
            } else {
                ASSET_PACK_IMPORTS.to_owned()
            },
        ),
        (
            "asset_pack_methods",
            if packs.is_empty() {
                String::new()
            } else {
                ASSET_PACK_METHODS.to_owned()
            },
        ),
    ];

    let mut files = Vec::with_capacity(TEMPLATES.len() + 2);
    for (path, template) in TEMPLATES {
        let contents = render(template, &vars).with_context(|| format!("rendering template {path}"))?;
        files.push(GeneratedFile {
            path: (*path).to_owned(),
            contents,
        });
    }
    let package_dir = project.id.replace('.', "/");
    files.push(GeneratedFile {
        path: format!("app/src/main/java/{package_dir}/{ACTIVITY_CLASS}.java"),
        contents: render(ACTIVITY_TEMPLATE, &vars).context("rendering the activity template")?,
    });
    for pack in packs {
        files.push(GeneratedFile {
            path: format!("{ASSET_PACKS_DIR}/{}/build.gradle.kts", pack.pack),
            contents: asset_pack_build_file(pack),
        });
    }
    files.push(GeneratedFile {
        path: "local.properties".to_owned(),
        contents: format!(
            "# Generated by `regia package --platform android`; do not edit.\nsdk.dir={}\n",
            properties_escape(&toolchain.sdk_dir.to_string_lossy())
        ),
    });
    Ok(files)
}

/// Writes rendered files below `root`, creating directories as needed.
pub fn write_files(root: &Path, files: &[GeneratedFile]) -> Result<()> {
    for file in files {
        let path = file
            .path
            .split('/')
            .fold(root.to_path_buf(), |p, part| p.join(part));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let result = if owner_only_wanted(cfg!(windows), &file.contents) {
            write_private(&path, &file.contents)
        } else {
            std::fs::write(&path, &file.contents)
        };
        result.with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Whether `write_files` writes a generated file owner-only (`windows`: the host is Windows).
///
/// Unix: every file that configures signing (`storePassword`) gets mode 0600, as before; it
/// costs nothing and cannot fail. Windows: only a file that holds a literal secret
/// (`holds_literal_secret`) gets the protected owner-only DACL, which replaces the folder's
/// inherited entries; an `env:VAR` file holds no secret and keeps the folder's ACL, so a build
/// that only reads secrets from the environment does not depend on it (review
/// sprint-rev-v05-windows-close finding 2, ADR-5616).
fn owner_only_wanted(windows: bool, contents: &str) -> bool {
    if windows {
        holds_literal_secret(contents)
    } else {
        contents.contains("storePassword")
    }
}

/// Whether a generated Gradle file holds a literal signing secret: a `storePassword` or
/// `keyPassword` assignment whose value is anything but the `System.getenv(..)` lookup that
/// `secret_expr` writes for an `env:VAR` reference. Anything unrecognised counts as a secret.
fn holds_literal_secret(contents: &str) -> bool {
    contents.lines().any(|line| {
        let line = line.trim_start();
        ["storePassword", "keyPassword"].iter().any(|key| {
            line.strip_prefix(key)
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('='))
                .is_some_and(|value| !value.trim_start().starts_with("(System.getenv("))
        })
    })
}

/// Writes a file holding signing secrets, readable by the owner only, through
/// `regia_net::private_file::create_private` (ADR-5616): mode `0600` on Unix; on Windows a DACL
/// for this process's account, `SYSTEM` and `Administrators`, set when the file is created and
/// opened with no sharing, so no other account can open it before the secret lands. An existing
/// file is removed first, so an older, wider permission does not survive. Other hosts get the
/// folder's permissions, and `secret_expr` warns about the literal.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(any(unix, windows))]
    {
        use std::io::Write;
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = regia_net::private_file::create_private(path)?;
        file.write_all(contents.as_bytes())
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(extra_android: &str) -> Result<Manifest> {
        Manifest::parse(&format!(
            "[project]\nname = \"Hello Regia Engine\"\nid = \"com.example.hello\"\nversion = \"1.2.3\"\n\
             build_number = 7\n[platforms.android]\n{extra_android}\n"
        ))
    }

    fn toolchain() -> Toolchain {
        Toolchain {
            sdk_dir: PathBuf::from("/opt/android-sdk"),
            ndk: Some(Ndk {
                dir: PathBuf::from("/opt/ndk/28"),
                version: Some("28.0.13004108".to_owned()),
            }),
        }
    }

    fn file<'a>(files: &'a [GeneratedFile], path: &str) -> &'a str {
        files
            .iter()
            .find(|f| f.path == path)
            .map_or("", |f| f.contents.as_str())
    }

    fn chunk_manifest(chunks: &[(&str, Delivery)]) -> ContentManifest {
        let mut m = ContentManifest::new("1.0-1-x", "android");
        for (name, delivery) in chunks {
            m.chunks.push(regia_asset::content::ChunkEntry {
                name: (*name).to_owned(),
                file: format!("{name}.fpak"),
                size: 1,
                hash: String::new(),
                delivery: *delivery,
                requires: Vec::new(),
                overrides: false,
                mount_order: 0,
                assets: 1,
                blocks: Vec::new(),
            });
        }
        m
    }

    /// CLI2 audit 9: a callback of an older request (the state query posted before a fetch, the
    /// failure listener of a fetch that a retry cancelled, the store's late `CANCELED` for the old
    /// download) must not overwrite the state of the newer fetch. Text checks: the Play library is
    /// not available to compile and run the activity here.
    fn assert_retry_callbacks_are_generation_checked(java: &str) {
        for needle in [
            // Every fetch moves the pack's generation on; one that overtakes a cancel is fresh.
            "gen = regiaPackGens.merge(pack, 1, Integer::sum);",
            "regiaPackFresh.add(pack);",
            // The fetch's failure listener writes only for its own generation.
            "if (regiaPackGens.getOrDefault(pack, 0) != gen) {",
            // A state query fills a missing row only, and only if no fetch came since.
            "asked = regiaPackGens.getOrDefault(pack, 0);",
            "if (state != null && regiaPackGens.getOrDefault(pack, 0) == asked && !regiaPackGone.contains(pack)) {",
            "regiaPackStates.putIfAbsent(pack, regiaPackRow(state));",
            // The store's listener drops the previous download's cancel after a retry.
            "if (status == AssetPackStatus.CANCELED && regiaPackFresh.contains(pack)) {",
            "regiaPacks.registerListener(this::regiaPackUpdate);",
        ] {
            assert!(java.contains(needle), "activity lacks `{needle}`");
        }
    }

    /// The Java statements of one generated method, from its signature to the next `public`/
    /// `private` member.
    fn java_method<'a>(java: &'a str, signature: &str) -> &'a str {
        let start = java.find(signature).unwrap_or_else(|| panic!("no `{signature}`"));
        let rest = &java[start + signature.len()..];
        let end = ["\n    public ", "\n    private "]
            .iter()
            .filter_map(|m| rest.find(m))
            .min()
            .unwrap_or(rest.len());
        &rest[..end]
    }

    /// CLI2 audit 9, the cases the generation test above does not reach (from the
    /// fix-cli2-correctness lane, re-cut onto v046a):
    /// - a fetch with no cancel pending is not marked fresh, so a cancel made before the store's
    ///   first state for it still lands (it used to be dropped as "the previous download's echo",
    ///   leaving the pack PENDING for good);
    /// - a cancel ends the fresh mark, records a pending cancel, and moves the generation on, so
    ///   the cancelled fetch's late failure callback writes nothing;
    /// - a remove moves the generation on, so a state query posted before it cannot put the removed
    ///   pack's row back, and it clears both marks;
    /// - the store's CANCELED that is accepted settles the pending cancel.
    #[test]
    fn asset_pack_cancel_marks_follow_the_cancel_not_every_fetch() -> Result<()> {
        let packs = asset_pack_modules(&chunk_manifest(&[("level", Delivery::OnDemand)]))?;
        let files = render_project(&manifest("")?, &toolchain(), &packs)?;
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        let bump = "regiaPackGens.merge(pack, 1, Integer::sum)";

        let fetch = java_method(java, "public void regiaPackFetch(final String pack)");
        assert!(
            fetch.contains(
                "if (regiaPackCancelled.remove(pack)) {\n                regiaPackFresh.add(pack);\n            }"
            ),
            "a fetch is fresh only when it overtakes a pending cancel:\n{fetch}"
        );
        assert_eq!(fetch.matches("regiaPackFresh.add(pack)").count(), 1, "{fetch}");

        let cancel = java_method(java, "public void regiaPackCancel(String pack)");
        for needle in [
            bump,
            "regiaPackFresh.remove(pack);",
            "regiaPackCancelled.add(pack);",
        ] {
            assert!(cancel.contains(needle), "cancel lacks `{needle}`:\n{cancel}");
        }
        assert!(
            matches!(
                (cancel.find("regiaPackCancelled.add(pack)"), cancel.find("regiaPackManager().cancel(")),
                (Some(mark), Some(call)) if mark < call
            ),
            "the cancel is recorded before the store can report it:\n{cancel}"
        );

        let remove = java_method(java, "public void regiaPackRemove(String pack)");
        for needle in [
            bump,
            "regiaPackFresh.remove(pack);",
            "regiaPackCancelled.remove(pack);",
        ] {
            assert!(remove.contains(needle), "remove lacks `{needle}`:\n{remove}");
        }
        assert!(
            matches!(
                (remove.find(bump), remove.find("regiaPackStates.remove(pack)")),
                (Some(bumped), Some(clear)) if bumped < clear
            ),
            "the generation moves before the row is cleared:\n{remove}"
        );

        let update = java_method(java, "private void regiaPackUpdate(AssetPackState state)");
        let drop = update.find("regiaPackFresh.contains(pack)").expect("echo filter");
        let settle = update
            .find("regiaPackCancelled.remove(pack)")
            .expect("an accepted CANCELED settles the pending cancel");
        assert!(drop < settle, "the echo filter runs first:\n{update}");
        assert_eq!(
            java.matches('{').count(),
            java.matches('}').count(),
            "balanced braces"
        );
        assert_cancel_marks_end_with_the_download(java);
        Ok(())
    }

    /// Review finding 3 (sprint-rev-fix-cli2-recut): the store cancels active downloads only, so a
    /// cancel of a pack that is already COMPLETED, FAILED, NOT_INSTALLED or CANCELED gets no
    /// CANCELED from the listener. Its mark must not be left behind for the next fetch to turn into
    /// a fresh mark (which would drop that fetch's real CANCELED and leave the pack PENDING):
    /// - a cancel marks the pack only while its known state is not terminal (an unknown state is
    ///   still marked, as the download may be running);
    /// - any terminal state the listener accepts (not only CANCELED) settles the mark, so a
    ///   download that completes or fails while its cancel is in flight does not keep it.
    fn assert_cancel_marks_end_with_the_download(java: &str) {
        let terminal = java_method(java, "private static boolean regiaPackTerminal(int status)");
        for status in ["COMPLETED", "FAILED", "NOT_INSTALLED", "CANCELED"] {
            assert!(
                terminal.contains(format!("status == AssetPackStatus.{status}").as_str()),
                "{status} is not a terminal state:\n{terminal}"
            );
        }
        for live in ["PENDING", "DOWNLOADING", "TRANSFERRING", "WAITING_FOR_WIFI"] {
            assert!(
                !terminal.contains(format!("AssetPackStatus.{live}").as_str()),
                "{live} is a live state:\n{terminal}"
            );
        }

        let update = java_method(java, "private void regiaPackUpdate(AssetPackState state)");
        let drop = update.find("regiaPackFresh.contains(pack)").expect("echo filter");
        let settled = update
            .find("if (regiaPackTerminal(status)) {")
            .unwrap_or_else(|| panic!("a terminal state does not settle the cancel mark:\n{update}"));
        let settle = update.find("regiaPackCancelled.remove(pack)").expect("settle");
        assert!(
            drop < settled && settled < settle,
            "the echo filter runs first, then any terminal state settles the mark:\n{update}"
        );
        assert_eq!(
            update.matches("regiaPackCancelled.remove(pack)").count(),
            1,
            "{update}"
        );

        let cancel = java_method(java, "public void regiaPackCancel(String pack)");
        let known = cancel
            .find("long[] row = regiaPackStates.get(pack);")
            .unwrap_or_else(|| panic!("the cancel does not read the known state:\n{cancel}"));
        let guard = cancel
            .find("if (row == null || !regiaPackTerminal((int) row[0])) {")
            .unwrap_or_else(|| panic!("a settled pack is marked cancelled:\n{cancel}"));
        let mark = cancel.find("regiaPackCancelled.add(pack)").expect("mark");
        assert!(known < guard && guard < mark, "{cancel}");
    }

    /// Byte ranges of every `synchronized (regiaPackLock) { ... }` block in `java`.
    fn lock_ranges(java: &str) -> Vec<(usize, usize)> {
        let open = "synchronized (regiaPackLock) {";
        java.match_indices(open)
            .map(|(start, _)| (start, closing_brace(java, start + open.len() - 1)))
            .collect()
    }

    /// The index of the `}` matching the `{` at `open`.
    fn closing_brace(java: &str, open: usize) -> usize {
        let mut depth = 0usize;
        for (i, c) in java[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return open + i;
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced brace at {open}");
    }

    /// Review finding 4 (sprint-rev-fix-cli2-recut): the listener thread (`regiaPackUpdate`) and a
    /// JNI caller (`regiaPackFetch` and the other helpers) each read a mark and then write a row;
    /// without one lock a retry's PENDING can land between the listener's check and its write,
    /// which then overwrites it with the old download's CANCELED. Every read or write of the
    /// pack bookkeeping (states, generations, both marks, the manager field) must sit inside a
    /// `synchronized (regiaPackLock)` block that starts in the same method body or callback
    /// lambda (a lock held by the method that registered a callback is not held when it runs),
    /// and no other monitor guards it.
    /// ADR-7696: the listener logs the store's status to logcat (tag `regia_pack`) once per change
    /// of an accepted row, never per byte update, and before the row replaces the shown one.
    #[test]
    fn asset_pack_listener_logs_store_status_changes_only() -> Result<()> {
        let packs = asset_pack_modules(&chunk_manifest(&[("level", Delivery::OnDemand)]))?;
        let files = render_project(&manifest("")?, &toolchain(), &packs)?;
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        let update = java
            .find("private void regiaPackUpdate(AssetPackState state) {")
            .expect("the listener");
        let body = &java[update..closing_brace(java, java[update..].find('{').expect("body") + update)];
        assert_eq!(
            body.matches("android.util.Log.i(\"regia_pack\"").count(),
            1,
            "{body}"
        );
        let guard = body
            .find("if (shown == null || shown[0] != status) {")
            .expect("the log is gated on a status change");
        let log = body.find("android.util.Log.i(").expect("log");
        let store = body
            .find("regiaPackStates.put(pack, regiaPackRow(state));")
            .expect("row store");
        assert!(
            guard < log && log < store,
            "gate, then log, then the row replaces the shown one"
        );
        // Rows the listener drops (removed, cancelled here, the old download's echo) are not logged.
        for early in ["regiaPackGone.contains(pack)", "regiaPackFresh.contains(pack)) {"] {
            assert!(body.find(early).expect("early return") < log, "{early}");
        }
        Ok(())
    }

    #[test]
    fn asset_pack_bookkeeping_is_under_one_lock() -> Result<()> {
        let packs = asset_pack_modules(&chunk_manifest(&[("level", Delivery::OnDemand)]))?;
        let files = render_project(&manifest("")?, &toolchain(), &packs)?;
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        assert!(
            java.contains("private final Object regiaPackLock = new Object();"),
            "no dedicated lock"
        );
        assert_eq!(
            java.matches("synchronized").count(),
            java.matches("synchronized (regiaPackLock) {").count(),
            "a second monitor (a synchronized method or block locks something else)"
        );

        let locks = lock_ranges(java);
        let lambdas: Vec<(usize, usize)> = java
            .match_indices("-> {")
            .map(|(start, m)| (start, closing_brace(java, start + m.len() - 1)))
            .collect();
        let mut checked = 0;
        for field in [
            "regiaPackStates",
            "regiaPackGens",
            "regiaPackCancelled",
            "regiaPackFresh",
            "regiaPacks",
        ] {
            for (at, _) in java.match_indices(field) {
                // Whole identifiers only (`regiaPacks` is not `regiaPackStates`).
                if java[at + field.len()..].starts_with(|c: char| c.is_ascii_alphanumeric()) {
                    continue;
                }
                let line_start = java[..at].rfind('\n').map_or(0, |i| i + 1);
                let line = java[line_start..].lines().next().unwrap_or("").trim_start();
                if line.starts_with("private ") || line.starts_with("//") || line.starts_with('*') {
                    continue; // The field declaration, or a comment.
                }
                // The innermost callback lambda around this use: its body runs later, on another
                // thread, so the lock must be taken inside it.
                let scope = lambdas
                    .iter()
                    .filter(|(s, e)| *s < at && at < *e)
                    .map(|(s, _)| *s)
                    .max()
                    .unwrap_or(0);
                let method_start = java[..at].rfind("\n    p").unwrap_or(0).max(scope);
                assert!(
                    locks.iter().any(|(s, e)| *s > method_start && *s < at && at < *e),
                    "`{line}` touches pack bookkeeping outside `synchronized (regiaPackLock)`"
                );
                checked += 1;
            }
        }
        assert!(checked >= 27, "only {checked} uses found");

        for signature in [
            "private AssetPackManager regiaPackManager()",
            "private void regiaPackUpdate(AssetPackState state)",
            "public long[] regiaPackState(final String pack)",
            "public void regiaPackFetch(final String pack)",
            "public void regiaPackCancel(String pack)",
            "public void regiaPackRemove(String pack)",
        ] {
            assert!(
                java_method(java, signature).contains("synchronized (regiaPackLock) {"),
                "`{signature}` does not hold the pack lock"
            );
        }
        // The two store callbacks that check a generation and then write take the lock themselves.
        for signature in [
            "public long[] regiaPackState(final String pack)",
            "public void regiaPackFetch(final String pack)",
        ] {
            assert_eq!(
                java_method(java, signature)
                    .matches("synchronized (regiaPackLock) {")
                    .count(),
                2,
                "`{signature}`'s store callback does not hold the pack lock"
            );
        }
        Ok(())
    }

    /// A remove must stay gone when the store listener delivers another state, and a cancel of
    /// the current fetch must be recorded. Text checks: the Play library is not compiled here.
    fn assert_removed_and_canceled_packs_stay_down(java: &str) {
        for needle in [
            "private final Set<String> regiaPackGone = ConcurrentHashMap.newKeySet();",
            "private final Set<String> regiaPackLocalCancel = ConcurrentHashMap.newKeySet();",
            "if (regiaPackGone.contains(pack) || regiaPackLocalCancel.contains(pack)) {",
            "regiaPackGone.add(pack);",
            "regiaPackGone.remove(pack);",
            "regiaPackLocalCancel.add(pack);",
            "regiaPackLocalCancel.remove(pack);",
            "regiaPackFresh.remove(pack);",
            "new long[] {AssetPackStatus.CANCELED, 0, 0, 0}",
            "&& !regiaPackGone.contains(pack)",
        ] {
            assert!(java.contains(needle), "activity lacks `{needle}`");
        }
    }

    /// Symbolic statuses mirroring `AssetPackStatus` for [`PackTracker`] (the values only need
    /// to be distinct; the tracker never leaves Rust).
    const NOT_INSTALLED: i64 = 0;
    const PENDING: i64 = 1;
    const DOWNLOADING: i64 = 2;
    const COMPLETED: i64 = 3;
    const FAILED: i64 = 4;
    const CANCELED: i64 = 5;

    /// Mirrors `regiaPackTerminal` in the generated activity.
    fn terminal(status: i64) -> bool {
        matches!(status, COMPLETED | FAILED | NOT_INSTALLED | CANCELED)
    }

    /// How the generated `regiaPackUpdate` ends the fresh (echo) mark, read off the generated
    /// text so the behaviour check below pins the rule the shipped Java actually implements.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FreshRule {
        /// The rejected audfix2 rule: a live row is stored but the mark stays armed until a
        /// CANCELED arrives, so a real cancel of the retry is swallowed as "the echo".
        ArmedUntilEcho,
        /// The first accepted row clears the mark (v048's rule on v048's state machine).
        ClearedOnAcceptedRow,
        /// No listener row ever clears the mark: every later CANCELED is dropped as "the
        /// echo" forever. Rejected like `ArmedUntilEcho` — the fixrev2 review's mutation probe
        /// (delete the listener's `regiaPackFresh.remove(pack);`) leaves the model passing
        /// unless this variant is ruled out in the generated text.
        NeverCleared,
    }

    /// Derives the fresh rule from the generated listener: the rejected armed rule guards the
    /// row store on the mark still being set, after the echo filter, and returns inside the
    /// guard; the accepted rule clears the mark after the echo filter and before the row store.
    fn fresh_rule_of(update: &str) -> FreshRule {
        let echo = update
            .find("if (status == AssetPackStatus.CANCELED && regiaPackFresh.contains(pack)) {")
            .expect("the echo filter");
        let after_echo = &update[echo..];
        if after_echo.contains("if (regiaPackFresh.contains(pack)) {") {
            return FreshRule::ArmedUntilEcho;
        }
        let row_store = after_echo
            .find("regiaPackStates.put(pack, regiaPackRow(state));")
            .expect("the row store");
        if after_echo[..row_store].contains("regiaPackFresh.remove(pack);") {
            FreshRule::ClearedOnAcceptedRow
        } else {
            FreshRule::NeverCleared
        }
    }

    /// A Rust mirror of the generated pack bookkeeping (the row and the cancel marks), stepped
    /// by the same helper calls and store rows as the Java. The listener takes the rule the
    /// generated Java actually implements ([`fresh_rule_of`]), so this is a behaviour check of
    /// the shipped text, not of a free-standing model. `row: None` mirrors a missing row (the
    /// Java reads `regiaPackStates.get(pack)` as null).
    #[derive(Debug, Default)]
    struct PackTracker {
        row: Option<i64>,
        cancelled: bool,
        fresh: bool,
        local_cancel: bool,
    }

    impl PackTracker {
        fn fetch(&mut self) {
            self.local_cancel = false;
            self.fresh = std::mem::take(&mut self.cancelled);
            self.row = Some(PENDING);
        }

        fn cancel(&mut self) {
            self.fresh = false;
            if self.row.is_some_and(terminal) {
                return; // The store cancels active downloads only: it will report nothing.
            }
            self.cancelled = true;
            self.local_cancel = true;
            self.row = Some(CANCELED);
        }

        fn listener(&mut self, status: i64, rule: FreshRule) {
            if self.local_cancel {
                return; // Rows of a locally cancelled request are ignored until the next fetch.
            }
            match rule {
                FreshRule::ArmedUntilEcho => {
                    if status == CANCELED && self.fresh {
                        self.fresh = false;
                        return;
                    }
                    if self.fresh {
                        self.row = Some(status); // Stored, and the mark stays armed.
                        return;
                    }
                    if terminal(status) {
                        self.cancelled = false;
                    }
                    self.row = Some(status);
                }
                FreshRule::ClearedOnAcceptedRow => {
                    if status == CANCELED && self.fresh {
                        return; // The cancelled previous download, reported after the retry.
                    }
                    if terminal(status) {
                        self.cancelled = false;
                    }
                    self.fresh = false;
                    self.row = Some(status);
                }
                FreshRule::NeverCleared => {
                    if status == CANCELED && self.fresh {
                        return; // Dropped as the echo; the mark stays set.
                    }
                    if terminal(status) {
                        self.cancelled = false;
                    }
                    self.row = Some(status); // Stored, and the mark stays set forever.
                }
            }
        }
    }

    /// Fixrev2 of cli-g finding 1 (sprint-rev-audfix2-a): the app cancels a download and the
    /// store's CANCELED echo is ignored under the local-cancel mark, so the pending-cancel mark
    /// is still there when a retry makes the pack fresh. A real cancel of the retry (the Play
    /// download notification, or a store-side cancel) must then land: the audfix2 rule kept the
    /// mark armed until an echo and swallowed it, leaving the last DOWNLOADING up forever.
    #[test]
    fn a_real_cancel_of_a_retry_is_not_swallowed_as_the_old_echo() -> Result<()> {
        let packs = asset_pack_modules(&chunk_manifest(&[("level", Delivery::OnDemand)]))?;
        let files = render_project(&manifest("")?, &toolchain(), &packs)?;
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        let rule = fresh_rule_of(java_method(
            java,
            "private void regiaPackUpdate(AssetPackState state)",
        ));
        assert_eq!(
            rule,
            FreshRule::ClearedOnAcceptedRow,
            "the listener must clear the fresh mark on the first accepted row: armed-until-echo \
             swallows a real cancel of the retry as the echo, and no clear at all swallows \
             every later cancel (cli-g finding 1)"
        );
        let mut pack = PackTracker::default();
        pack.fetch(); // The first download.
        pack.listener(DOWNLOADING, rule);
        pack.cancel(); // The app cancel; the store's echo is ignored under the local-cancel mark.
        pack.listener(CANCELED, rule);
        pack.fetch(); // The retry; the still-pending cancel mark makes it fresh.
        pack.listener(DOWNLOADING, rule);
        pack.listener(CANCELED, rule); // The user cancels the retry from the notification.
        assert_eq!(
            pack.row,
            Some(CANCELED),
            "the retry's real cancel was swallowed as the old download's echo"
        );
        Ok(())
    }

    /// The lock part of the replaced `audfix2_pack_flag_updates_share_one_lock` source-text
    /// test, kept as ordering checks: every flag update happens under `regiaPackLock`, and the
    /// store IPC runs after the lock is released, so the listener (which takes the same lock)
    /// never waits on a store call. What a live row does to the fresh mark is a behaviour
    /// check now (`a_real_cancel_of_a_retry_is_not_swallowed_as_the_old_echo`).
    #[test]
    fn pack_store_calls_run_after_the_lock_is_released() -> Result<()> {
        let packs = asset_pack_modules(&chunk_manifest(&[("level", Delivery::OnDemand)]))?;
        let files = render_project(&manifest("")?, &toolchain(), &packs)?;
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        let fetch = java_method(java, "public void regiaPackFetch(final String pack)");
        assert_locked_then_store(fetch, "regiaPackFresh.add(pack)", "regiaPackManager().fetch(");
        let cancel = java_method(java, "public void regiaPackCancel(String pack)");
        assert_locked_then_store(
            cancel,
            "regiaPackCancelled.add(pack)",
            "regiaPackManager().cancel(",
        );
        let update = java_method(java, "private void regiaPackUpdate(AssetPackState state)");
        assert_under_lock(update, "regiaPackFresh.contains(pack)");
        assert_under_lock(update, "regiaPackStates.put(pack");
        let remove = java_method(java, "public void regiaPackRemove(String pack)");
        assert_locked_then_store(
            remove,
            "regiaPackStates.remove(pack)",
            "regiaPackManager().removePack(",
        );
        Ok(())
    }

    /// Byte offset just past the first `synchronized (regiaPackLock)` block.
    fn pack_lock_end(method: &str) -> usize {
        let lock = method
            .find("synchronized (regiaPackLock)")
            .unwrap_or_else(|| panic!("no regiaPackLock:\n{method}"));
        let open = method[lock..].find('{').expect("lock block") + lock;
        let mut depth = 0usize;
        for (index, ch) in method[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return open + index;
                    }
                }
                _ => {}
            }
        }
        panic!("unclosed lock:\n{method}");
    }

    /// `marker` is inside the lock, and `call` (the store IPC) is after the lock is released.
    fn assert_locked_then_store(method: &str, marker: &str, call: &str) {
        let end = pack_lock_end(method);
        let at = method
            .find(marker)
            .unwrap_or_else(|| panic!("no `{marker}`:\n{method}"));
        let call_at = method
            .find(call)
            .unwrap_or_else(|| panic!("no `{call}`:\n{method}"));
        let lock = method.find("synchronized (regiaPackLock)").expect("lock");
        assert!(
            lock < at && at < end && end < call_at,
            "`{marker}` under the lock, `{call}` after it:\n{method}"
        );
    }

    /// `marker` is inside the first `synchronized (regiaPackLock)` block.
    fn assert_under_lock(method: &str, marker: &str) {
        let end = pack_lock_end(method);
        let lock = method.find("synchronized (regiaPackLock)").expect("lock");
        let at = method
            .find(marker)
            .unwrap_or_else(|| panic!("no `{marker}`:\n{method}"));
        assert!(lock < at && at < end, "`{marker}` under the lock:\n{method}");
    }

    /// Spec 40 § 8 test 21 (host half): a three-chunk project gets one asset-pack module per
    /// fast-follow / on-demand chunk with its delivery type, the includes, the app's `assetPacks`,
    /// the store library and the activity helpers native code calls.
    #[test]
    fn asset_pack_modules_render_for_store_chunks() -> Result<()> {
        let chunks = chunk_manifest(&[
            ("base", Delivery::Install),
            ("open-world", Delivery::OnDemand),
            ("intro", Delivery::FastFollow),
            ("dlc", Delivery::External),
        ]);
        let packs = asset_pack_modules(&chunks)?;
        assert_eq!(
            packs
                .iter()
                .map(|p| (p.pack.as_str(), p.delivery_type()))
                .collect::<Vec<_>>(),
            [("open_world", "on-demand"), ("intro", "fast-follow")]
        );
        assert_eq!(
            packs[0].asset_path(),
            "packs/open_world/src/main/assets/open-world.fpak"
        );
        let files = render_project(&manifest("")?, &toolchain(), &packs)?;
        for f in &files {
            assert!(!f.contents.contains("{{"), "placeholder left in {}", f.path);
        }
        let module = file(&files, "packs/open_world/build.gradle.kts");
        for expected in [
            "id(\"com.android.asset-pack\")",
            "packName.set(\"open_world\")",
            "deliveryType.set(\"on-demand\")",
        ] {
            assert!(module.contains(expected), "module lacks `{expected}`:\n{module}");
        }
        assert!(file(&files, "packs/intro/build.gradle.kts").contains("deliveryType.set(\"fast-follow\")"));
        let settings = file(&files, "settings.gradle.kts");
        assert!(settings.contains(
            "include(\":open_world\")\nproject(\":open_world\").projectDir = file(\"packs/open_world\")"
        ));
        assert!(settings.contains("include(\":intro\")"));
        let app = file(&files, "app/build.gradle.kts");
        assert!(
            app.contains("assetPacks += listOf(\":open_world\", \":intro\")"),
            "{app}"
        );
        assert!(
            app.contains("com.google.android.play:asset-delivery:2.2.2"),
            "{app}"
        );
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        for method in [
            "public long[] regiaPackState(final String pack)",
            "public void regiaPackFetch(final String pack)",
            "public void regiaPackCancel(String pack)",
            "public void regiaPackRemove(String pack)",
            "public String regiaPackLocation(String pack)",
            "public void regiaPackConfirm()",
            "import com.google.android.play.core.assetpacks.AssetPackManagerFactory;",
        ] {
            assert!(java.contains(method), "activity lacks `{method}`");
        }
        assert_eq!(
            java.matches('{').count(),
            java.matches('}').count(),
            "balanced braces"
        );
        assert_retry_callbacks_are_generation_checked(java);
        assert_removed_and_canceled_packs_stay_down(java);

        // Without packs: no modules, no store library, no helpers.
        let plain = render_project(&manifest("")?, &toolchain(), &[])?;
        assert!(!plain.iter().any(|f| f.path.starts_with(ASSET_PACKS_DIR)));
        assert!(!file(&plain, "settings.gradle.kts").contains("projectDir"));
        assert!(!file(&plain, "app/build.gradle.kts").contains("asset-delivery"));
        assert!(
            !file(&plain, "app/src/main/java/com/example/hello/RegiaActivity.java").contains("regiaPack")
        );
        Ok(())
    }

    #[test]
    fn a_chunk_named_like_the_app_module_is_refused() {
        let err = asset_pack_modules(&chunk_manifest(&[
            ("base", Delivery::Install),
            ("app", Delivery::OnDemand),
        ]))
        .expect_err("collides with :app");
        assert!(err.to_string().starts_with("pack_name_collision"), "{err}");
        let err = asset_pack_modules(&chunk_manifest(&[
            ("a-b", Delivery::OnDemand),
            ("a_b", Delivery::FastFollow),
        ]))
        .expect_err("same pack");
        assert!(err.to_string().starts_with("pack_name_collision"), "{err}");
    }

    #[test]
    fn render_replaces_and_rejects_leftovers() -> Result<()> {
        assert_eq!(render("a {{x}} b {{x}}", &[("x", "1".into())])?, "a 1 b 1");
        let err = render("a {{x}} {{missing}}", &[("x", "1".into())]).err();
        assert!(err.is_some_and(|e| e.to_string().contains("{{missing}}")));
        Ok(())
    }

    #[test]
    fn escaping() {
        assert_eq!(xml_escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
        assert_eq!(
            android_string_escape("Bob's \"Game\" & co"),
            "Bob\\&apos;s \\&quot;Game\\&quot; &amp; co"
        );
        assert_eq!(android_string_escape("@string/x"), "\\@string/x");
        assert_eq!(android_string_escape("?a @b"), "\\?a @b");
        assert_eq!(kotlin_string_escape("C:\\x\\$y\"z"), "C:\\\\x\\\\\\$y\\\"z");
        assert_eq!(
            properties_escape("C:\\Users\\me\\Sdk"),
            "C\\:\\\\Users\\\\me\\\\Sdk"
        );
        assert_eq!(properties_escape("/home/me/Android/Sdk"), "/home/me/Android/Sdk");
    }

    #[test]
    fn project_files_have_no_placeholders_and_expected_paths() -> Result<()> {
        let files = render_project(&manifest("")?, &toolchain(), &[])?;
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        for expected in [
            "settings.gradle.kts",
            "build.gradle.kts",
            "gradle.properties",
            "local.properties",
            "app/build.gradle.kts",
            "app/src/main/AndroidManifest.xml",
            "app/src/main/res/values/strings.xml",
            "app/src/main/res/values/themes.xml",
            "app/src/main/java/com/example/hello/RegiaActivity.java",
        ] {
            assert!(paths.contains(&expected), "missing {expected} in {paths:?}");
        }
        for f in &files {
            assert!(!f.contents.contains("{{"), "placeholder left in {}", f.path);
        }
        // Everything under the generated source dir must be a known template path: nothing extra,
        // nothing missing (the dir is wiped before each build).
        let mut generated: Vec<&str> = paths
            .iter()
            .copied()
            .filter(|p| p.starts_with(GENERATED_SOURCE_DIR))
            .collect();
        generated.sort_unstable();
        assert_eq!(
            generated,
            [
                "app/src/debug/AndroidManifest.xml",
                "app/src/main/AndroidManifest.xml",
                "app/src/main/java/com/example/hello/RegiaActivity.java",
                "app/src/main/res/values/strings.xml",
                "app/src/main/res/values/themes.xml",
            ]
        );
        Ok(())
    }

    #[test]
    fn gradle_build_values() -> Result<()> {
        let files = render_project(
            &manifest("min_sdk = 30\ntarget_sdk = 36\nabis = [\"arm64-v8a\", \"x86_64\"]")?,
            &toolchain(),
            &[],
        )?;
        let app = file(&files, "app/build.gradle.kts");
        for expected in [
            "namespace = \"com.example.hello\"",
            "applicationId = \"com.example.hello\"",
            "compileSdk = 36",
            "targetSdk = 36",
            "minSdk = 30",
            "versionCode = 7",
            "versionName = \"1.2.3\"",
            "abiFilters += listOf(\"arm64-v8a\", \"x86_64\")",
            "noCompress += \"fpak\"",
            "ndkPath = \"/opt/ndk/28\"",
            "ndkVersion = \"28.0.13004108\"",
            "JavaVersion.VERSION_17",
            "androidx.games:games-activity:4.4.0\"",
            "androidx.appcompat:appcompat:1.7.0\"",
            "platform(\"org.jetbrains.kotlin:kotlin-bom:1.8.22\")",
        ] {
            assert!(
                app.contains(expected),
                "app/build.gradle.kts lacks `{expected}`:\n{app}"
            );
        }
        assert!(
            !app.contains("prefab = true") && !app.contains("externalNativeBuild"),
            "no prefab, no CMake"
        );
        assert!(file(&files, "build.gradle.kts").contains("version \"8.13.2\""));
        assert!(file(&files, "local.properties").contains("sdk.dir=/opt/android-sdk\n"));

        let without_ndk = Toolchain {
            ndk: None,
            ..toolchain()
        };
        let files = render_project(&manifest("")?, &without_ndk, &[])?;
        assert!(!file(&files, "app/build.gradle.kts").contains("ndkPath"));
        Ok(())
    }

    #[test]
    fn android_manifest_values() -> Result<()> {
        let files = render_project(
            &manifest("orientation = \"portrait\"\npermissions = [\"android.permission.INTERNET\"]")?,
            &toolchain(),
            &[],
        )?;
        let xml = file(&files, "app/src/main/AndroidManifest.xml");
        for expected in [
            "android:name=\".RegiaActivity\"",
            "android:screenOrientation=\"portrait\"",
            "android:launchMode=\"singleTask\"",
            "android:name=\"android.app.lib_name\"",
            "android:value=\"main\"",
            "android:appCategory=\"game\"",
            "android:isGame=\"true\"",
            "<uses-permission android:name=\"android.permission.INTERNET\" />",
            "android.intent.category.LAUNCHER",
            // The packager writes `ic_launcher` into every mipmap density (ADR-5627).
            "android:icon=\"@mipmap/ic_launcher\"",
        ] {
            assert!(
                xml.contains(expected),
                "AndroidManifest.xml lacks `{expected}`:\n{xml}"
            );
        }
        // A recreate would run android_main a second time and the winit loop can be built once, so
        // every user-changeable configuration (font size, language, direction, ...) is handled in place.
        let config_changes = xml
            .split("android:configChanges=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default();
        let tokens: Vec<&str> = config_changes.split('|').collect();
        for token in [
            "orientation",
            "screenSize",
            "screenLayout",
            "keyboardHidden",
            "keyboard",
            "uiMode",
            "density",
            "smallestScreenSize",
            "fontScale",
            "locale",
            "layoutDirection",
            "navigation",
            "colorMode",
        ] {
            assert!(
                tokens.contains(&token),
                "configChanges lacks `{token}`: {config_changes}"
            );
        }
        let java = file(&files, "app/src/main/java/com/example/hello/RegiaActivity.java");
        assert!(java.contains("package com.example.hello;"));
        assert!(java.contains("extends GameActivity"));
        assert!(java.contains("import com.google.androidgamesdk.GameActivity;"));
        let themes = file(&files, "app/src/main/res/values/themes.xml");
        assert!(themes.contains("parent=\"Theme.AppCompat.NoActionBar\""));
        // A resource name with a space fails Gradle's resource merge.
        assert!(themes.contains("<style name=\"Theme.Regia\""), "{themes}");
        assert!(file(&files, "app/src/main/AndroidManifest.xml").contains("@style/Theme.Regia\""));
        assert!(themes.contains("windowFullscreen"));
        assert!(file(&files, "app/src/main/res/values/strings.xml").contains(">Hello Regia Engine</string>"));
        Ok(())
    }

    /// The values of `attr="..."` in `xml`.
    fn attr_values<'a>(xml: &'a str, attr: &str) -> Vec<&'a str> {
        let key = format!("{attr}=\"");
        xml.match_indices(&key)
            .filter_map(|(at, _)| xml[at + key.len()..].split('"').next())
            .collect()
    }

    /// aapt2 accepts resource names of letters, digits, `_` and `.` only (ADR-1749: the brand
    /// rename once wrote `Theme.Regia Engine` and every APK build failed in `mergeDebugResources`).
    #[test]
    fn style_names_are_valid_resource_names_and_the_manifest_theme_exists() -> Result<()> {
        let files = render_project(&manifest("")?, &toolchain(), &[])?;
        let valid = |name: &str| {
            name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        };
        let mut defined = Vec::new();
        for f in files.iter().filter(|f| f.path.ends_with(".xml")) {
            for name in attr_values(&f.contents, "<style name") {
                assert!(
                    valid(name),
                    "{}: style name `{name}` is not a valid resource name",
                    f.path
                );
                defined.push(name.to_owned());
            }
        }
        assert!(!defined.is_empty(), "the project defines its theme");
        let manifest_xml = file(&files, "app/src/main/AndroidManifest.xml");
        let themes = attr_values(manifest_xml, "android:theme");
        assert!(!themes.is_empty(), "the manifest names a theme");
        for theme in themes {
            let name = theme.strip_prefix("@style/").unwrap_or(theme);
            assert!(
                valid(name),
                "manifest theme `{theme}` is not a valid resource name"
            );
            assert!(
                defined.iter().any(|d| d == name),
                "manifest theme `{theme}` is not defined (defined: {defined:?})"
            );
        }
        Ok(())
    }

    #[test]
    fn debug_builds_get_the_agent_socket_and_show_when_locked() -> Result<()> {
        let files = render_project(&manifest("")?, &toolchain(), &[])?;
        let debug = file(&files, "app/src/debug/AndroidManifest.xml");
        for expected in [
            "<uses-permission android:name=\"android.permission.INTERNET\" />",
            "android:showWhenLocked=\"true\"",
            "android:turnScreenOn=\"true\"",
            "tools:node=\"merge\"",
        ] {
            assert!(
                debug.contains(expected),
                "debug manifest lacks `{expected}`:\n{debug}"
            );
        }
        // Shipping builds (main source set) ask for neither.
        let main = file(&files, "app/src/main/AndroidManifest.xml");
        assert!(
            !main.contains("INTERNET") && !main.contains("showWhenLocked"),
            "{main}"
        );
        Ok(())
    }

    #[test]
    fn xml_comments_are_well_formed() -> Result<()> {
        // XML forbids `--` inside comments; aapt2 rejects the file.
        let files = render_project(&manifest("")?, &toolchain(), &[])?;
        for f in files.iter().filter(|f| f.path.ends_with(".xml")) {
            let stripped = f.contents.replace("<!--", "").replace("-->", "");
            assert!(!stripped.contains("--"), "`--` in a comment of {}", f.path);
        }
        Ok(())
    }

    #[test]
    fn default_orientation_is_sensor_landscape_and_only_engine_permissions() -> Result<()> {
        let files = render_project(&manifest("")?, &toolchain(), &[])?;
        let xml = file(&files, "app/src/main/AndroidManifest.xml");
        assert!(xml.contains("android:screenOrientation=\"sensorLandscape\""));
        assert_eq!(xml.matches("uses-permission").count(), ENGINE_PERMISSIONS.len());
        assert!(xml.contains("android.permission.VIBRATE"));
        Ok(())
    }

    /// The metered-network query of content downloads needs `ACCESS_NETWORK_STATE` (a normal
    /// permission): every Android build declares it once, also when the project lists it too.
    #[test]
    fn every_build_declares_access_network_state_once() -> Result<()> {
        for extra in ["", "permissions = [\"android.permission.ACCESS_NETWORK_STATE\"]"] {
            let files = render_project(&manifest(extra)?, &toolchain(), &[])?;
            let xml = file(&files, "app/src/main/AndroidManifest.xml");
            assert_eq!(
                xml.matches("android.permission.ACCESS_NETWORK_STATE").count(),
                1,
                "{extra}"
            );
        }
        Ok(())
    }

    #[test]
    fn project_permissions_are_merged_without_duplicates() -> Result<()> {
        let files = render_project(
            &manifest("permissions = [\"android.permission.INTERNET\", \"android.permission.VIBRATE\"]")?,
            &toolchain(),
            &[],
        )?;
        let xml = file(&files, "app/src/main/AndroidManifest.xml");
        assert_eq!(xml.matches("android.permission.VIBRATE").count(), 1);
        assert!(xml.contains("android.permission.INTERNET"));
        Ok(())
    }

    #[test]
    fn write_files_creates_directories() -> Result<()> {
        let root = std::env::temp_dir().join(format!("regia_cli_test_{}_write", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        let files = render_project(&manifest("")?, &toolchain(), &[])?;
        write_files(&root, &files)?;
        let java = root.join("app/src/main/java/com/example/hello/RegiaActivity.java");
        assert!(std::fs::read_to_string(java)?.contains("package com.example.hello;"));
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn signing_secrets_file_is_owner_only() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let root = regia_test_utils::unique_temp_dir("regia_cli_gradle_private");
        let files = vec![GeneratedFile {
            path: "app/build.gradle.kts".to_string(),
            contents: "storePassword = \"x\"\n".to_string(),
        }];
        // A pre-existing world-readable file must not keep its mode.
        std::fs::create_dir_all(root.join("app"))?;
        std::fs::write(root.join("app/build.gradle.kts"), "old")?;
        std::fs::set_permissions(
            root.join("app/build.gradle.kts"),
            std::fs::Permissions::from_mode(0o644),
        )?;
        write_files(&root, &files)?;
        let mode = std::fs::metadata(root.join("app/build.gradle.kts"))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode {mode:o}");
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn signing_secrets_file_is_owner_only() -> Result<()> {
        let root = regia_test_utils::unique_temp_dir("regia_cli_gradle_private");
        let files = vec![GeneratedFile {
            path: "app/build.gradle.kts".to_string(),
            contents: "storePassword = \"x\"\n".to_string(),
        }];
        // A pre-existing file with the folder's inherited entries must not keep them.
        std::fs::create_dir_all(root.join("app"))?;
        let file = root.join("app").join("build.gradle.kts");
        std::fs::write(&file, "old")?;
        let before = crate::owner_only::acl_entries(&file)?;
        write_files(&root, &files)?;
        crate::owner_only::assert_owner_only(&file, &before)?;
        assert_eq!(std::fs::read_to_string(&file)?, files[0].contents);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    /// Review sprint-rev-v05-windows-close finding 2: the Windows owner-only DACL was applied to
    /// every signed build (`contains("storePassword")`), including the `env:VAR` form whose file
    /// holds no secret, so every signed package on a Windows host depended on it. On Windows it
    /// now runs only for a literal secret; Unix keeps mode 0600 for every signing file.
    #[test]
    fn owner_only_acl_is_skipped_on_windows_without_a_literal_secret() -> Result<()> {
        let env_only = render_signing_config(
            &manifest(
                "keystore = \"/keys/upload.jks\"\n\
                 keystore_password = \"env:REGIA_TEST_STOREPW\"\n\
                 key_alias = \"upload\"\n",
            )?
            .platforms
            .android,
        )?;
        assert!(env_only.contains("storePassword = (System.getenv("), "{env_only}");
        let literal = "            storePassword = \"s3cret\"\n";
        let literal_key =
            "            storePassword = (System.getenv(\"A\") ?: \"\")\n            keyPassword = \"k\"\n";
        assert!(
            !owner_only_wanted(true, &env_only),
            "an env-only signing file is not restricted on Windows"
        );
        assert!(owner_only_wanted(true, literal), "a literal store password is");
        assert!(owner_only_wanted(true, literal_key), "a literal key password is");
        assert!(!owner_only_wanted(true, "plain = 1\n"));
        // Unix: unchanged, every signing file gets mode 0600 (it cannot fail and costs nothing).
        assert!(owner_only_wanted(false, &env_only));
        assert!(owner_only_wanted(false, literal));
        assert!(!owner_only_wanted(false, "plain = 1\n"));
        Ok(())
    }

    #[test]
    fn signing_config_is_empty_without_a_keystore() -> Result<()> {
        assert_eq!(render_signing_config(&manifest("")?.platforms.android)?, "");
        Ok(())
    }

    #[test]
    fn env_passwords_stay_out_of_the_generated_file() -> Result<()> {
        let android = manifest(
            "keystore = \"/keys/upload.jks\"\n\
             keystore_password = \"env:REGIA_TEST_STOREPW\"\n\
             key_alias = \"upload\"\n",
        )?
        .platforms
        .android;
        let block = render_signing_config(&android)?;
        assert!(
            block.contains("storePassword = (System.getenv(\"REGIA_TEST_STOREPW\") ?: \"\")"),
            "{block}"
        );
        // key_password defaults to the keystore password expression.
        assert!(
            block.contains("keyPassword = (System.getenv(\"REGIA_TEST_STOREPW\") ?: \"\")"),
            "{block}"
        );
        Ok(())
    }

    #[test]
    fn signing_config_renders_the_release_keystore() -> Result<()> {
        let android = manifest(
            "keystore = \"/keys/upload.jks\"\n\
             keystore_password = \"s3cret\"\n\
             key_alias = \"upload\"\n",
        )?
        .platforms
        .android;
        let block = render_signing_config(&android)?;
        for expected in [
            "signingConfigs {",
            "create(\"release\") {",
            "storeFile = file(\"/keys/upload.jks\")",
            "storePassword = \"s3cret\"",
            "keyAlias = \"upload\"",
            // key_password defaults to the keystore password.
            "keyPassword = \"s3cret\"",
            "signingConfig = signingConfigs.getByName(\"release\")",
        ] {
            assert!(block.contains(expected), "missing `{expected}`:\n{block}");
        }

        // The full project embeds it in app/build.gradle.kts.
        let files = render_project(
            &manifest(
                "keystore = \"/keys/upload.jks\"\nkeystore_password = \"s3cret\"\nkey_alias = \"upload\"\n",
            )?,
            &toolchain(),
            &[],
        )?;
        let app = file(&files, "app/build.gradle.kts");
        assert!(app.contains("storeFile = file(\"/keys/upload.jks\")"), "{app}");
        Ok(())
    }

    #[test]
    fn signing_config_rejects_an_unresolved_key_password() -> Result<()> {
        let android = manifest(
            "keystore = \"/keys/upload.jks\"\n\
             keystore_password = \"s3cret\"\n\
             key_alias = \"upload\"\n\
             key_password = \"env:REGIA_TEST_UNSET_KEY_PW_XYZ\"\n",
        )?
        .platforms
        .android;
        let err = render_signing_config(&android).expect_err("unset env key password must fail");
        assert!(err.to_string().contains("key_password"), "{err}");
        Ok(())
    }
}
