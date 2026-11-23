// SPDX-License-Identifier: MIT-0
//! Android specifics: where packages live and the launch intent's extras.
//!
//! `adb shell am start -n <app>/.RegiaActivity --es regia_package_url https://...` downloads a package,
//! `--es regia_package <slug>` opens one that is already here. `adb push <folder>
//! /sdcard/Android/data/<app>/files/packages/` copies one in; that folder exists after the first launch.

use std::path::PathBuf;

use jni::objects::{JObject, JString};
use jni::{JValue, JavaVM, jni_sig, jni_str};
use regia_platform::AndroidApp;

use crate::app::ViewerConfig;
use crate::surface::BackendChoice;

/// The launch intent's string extra `name`, if set.
fn intent_extra(app: &AndroidApp, name: &str) -> Option<String> {
    let vm = match JavaVM::singleton() {
        Ok(vm) => vm,
        Err(err) => {
            tracing::warn!(target: "product_viewer", "intent extra {name}: {err}");
            return None;
        }
    };
    let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
    let result = vm.attach_current_thread(|env| -> jni::errors::Result<Option<String>> {
        // SAFETY: `activity_as_ptr` is a JNI global reference owned by android-activity, valid while its
        // `AndroidApp` lives, which the caller holds for this whole call; the `Cast` wrapper never deletes
        // the reference (the same use as `regia_platform`'s `jvm.rs`).
        #[allow(unsafe_code)]
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw_activity)? };
        let intent = env
            .call_method(
                &activity,
                jni_str!("getIntent"),
                jni_sig!("()Landroid/content/Intent;"),
                &[],
            )?
            .l()?;
        if intent.is_null() {
            return Ok(None);
        }
        let key = env.new_string(name)?;
        let value = env
            .call_method(
                &intent,
                jni_str!("getStringExtra"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&key)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let text = env.cast_local::<JString>(value)?;
        Ok(Some(text.try_to_string(env)?))
    });
    match result {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(target: "product_viewer", "intent extra {name}: {err}");
            None
        }
    }
}

/// `--es regia_backend gl|vulkan|auto`; anything else is logged and means `auto`.
fn backend_choice(name: Option<&str>) -> BackendChoice {
    let Some(name) = name else {
        return BackendChoice::Auto;
    };
    BackendChoice::parse(name).unwrap_or_else(|| {
        tracing::warn!(target: "product_viewer", "intent extra regia_backend: unknown value {name}");
        BackendChoice::Auto
    })
}

/// Package folders: internal storage takes downloads, the external app folder takes `adb push`.
pub(crate) fn config(app: &AndroidApp) -> ViewerConfig {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(internal) = app.internal_data_path() {
        roots.push(internal.join("packages"));
    }
    let external = app.external_data_path().map(|dir| dir.join("packages"));
    if let Some(external) = &external {
        roots.push(external.clone());
    }
    ViewerConfig {
        package_roots: roots,
        fetch_url: intent_extra(app, "regia_package_url"),
        open_package: intent_extra(app, "regia_package"),
        backend: backend_choice(intent_extra(app, "regia_backend").as_deref()),
        samples: intent_extra(app, "regia_samples").and_then(|n| crate::app::parse_samples(&n)),
        pipeline_cache_dir: app.internal_data_path().map(|dir| dir.join("pipeline-cache")),
        push_hint: external
            .map(|dir| format!("adb push <folder> {}", dir.display()))
            .unwrap_or_default(),
    }
}
