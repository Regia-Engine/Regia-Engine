// SPDX-License-Identifier: MPL-2.0
//! The JNI calls behind the Android services. Each public function attaches the calling thread to
//! the JVM (cheap when already attached), runs inside a local reference frame, and turns any JNI
//! error or Java exception into a [`ServiceError`]; callers log and fall back.
//!
//! These calls run on the calling thread (the `android_main` thread or an ECS worker), not on the
//! Java UI thread. All of them are read-only queries or thread-safe framework calls
//! (`startActivity`, `Vibrator.vibrate`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use jni::errors::Error as JniError;
use jni::objects::{JObject, JString};
use jni::strings::JNIStr;
use jni::{Env, JValue, JavaVM, jni_sig, jni_str};

use crate::services::{SafeArea, ServiceError};

fn os_error(operation: &'static str, err: impl std::fmt::Display) -> ServiceError {
    ServiceError::Os {
        operation,
        message: err.to_string(),
    }
}

/// Runs `f` with an attached `Env` and the activity object.
fn with_activity<T>(
    operation: &'static str,
    f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> Result<T, JniError>,
) -> Result<T, ServiceError> {
    let app = super::app().ok_or(ServiceError::Unsupported(
        "Android activity not initialized (PlatformPlugin not built)",
    ))?;
    // android-activity initialises the jni crate's JavaVM singleton before `android_main` runs.
    let vm = JavaVM::singleton().map_err(|e| os_error(operation, e))?;
    let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
    vm.attach_current_thread(|env| -> Result<T, JniError> {
        // SAFETY: `activity_as_ptr` is a JNI global reference owned by android-activity, valid while
        // its `AndroidApp` lives; we hold a clone of `app` for the whole call, and the `Cast` wrapper
        // never deletes the reference.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw_activity)? };
        f(env, &activity)
    })
    .map_err(|e| os_error(operation, e))
}

fn to_string(env: &mut Env<'_>, obj: JObject<'_>) -> Result<String, JniError> {
    if obj.is_null() {
        return Ok(String::new());
    }
    let s = env.cast_local::<JString>(obj)?;
    let chars = s.mutf8_chars(env)?;
    Ok(String::from(chars))
}

fn static_string(env: &mut Env<'_>, class: &JNIStr, field: &JNIStr) -> Result<String, JniError> {
    let obj = env
        .get_static_field(class, field, jni_sig!("Ljava/lang/String;"))?
        .l()?;
    to_string(env, obj)
}

/// `Build.VERSION.SDK_INT`, cached (0 = not read yet).
static SDK_INT: AtomicI32 = AtomicI32::new(0);

fn sdk_int(env: &mut Env<'_>) -> Result<i32, JniError> {
    let cached = SDK_INT.load(Ordering::Relaxed);
    if cached > 0 {
        return Ok(cached);
    }
    let sdk = env
        .get_static_field(
            jni_str!("android/os/Build$VERSION"),
            jni_str!("SDK_INT"),
            jni_sig!("I"),
        )?
        .i()?;
    SDK_INT.store(sdk, Ordering::Relaxed);
    Ok(sdk)
}

/// `getWindowManager().getDefaultDisplay().getHdrCapabilities()` (API 24+): desired max/min
/// luminance in cd/m²; non-positive values (`INVALID_LUMINANCE`) and older APIs report `None`.
pub(super) fn display_luminance() -> Result<crate::services::DisplayLuminance, ServiceError> {
    with_activity("display_luminance", |env, activity| {
        let mut out = crate::services::DisplayLuminance::default();
        if sdk_int(env)? < 24 {
            return Ok(out);
        }
        let wm = env
            .call_method(
                activity,
                jni_str!("getWindowManager"),
                jni_sig!("()Landroid/view/WindowManager;"),
                &[],
            )?
            .l()?;
        if wm.is_null() {
            return Ok(out);
        }
        let display = env
            .call_method(
                &wm,
                jni_str!("getDefaultDisplay"),
                jni_sig!("()Landroid/view/Display;"),
                &[],
            )?
            .l()?;
        if display.is_null() {
            return Ok(out);
        }
        let caps = env
            .call_method(
                &display,
                jni_str!("getHdrCapabilities"),
                jni_sig!("()Landroid/view/Display$HdrCapabilities;"),
                &[],
            )?
            .l()?;
        if caps.is_null() {
            return Ok(out);
        }
        let peak = env
            .call_method(&caps, jni_str!("getDesiredMaxLuminance"), jni_sig!("()F"), &[])?
            .f()?;
        let min = env
            .call_method(&caps, jni_str!("getDesiredMinLuminance"), jni_sig!("()F"), &[])?
            .f()?;
        out.peak_nits = (peak.is_finite() && peak > 0.0).then_some(peak);
        out.min_nits = (min.is_finite() && min >= 0.0 && out.peak_nits.is_some()).then_some(min);
        Ok(out)
    })
}

/// `Context.getCacheDir().getAbsolutePath()`.
pub(super) fn cache_dir() -> Result<PathBuf, ServiceError> {
    let path = with_activity("getCacheDir", |env, activity| {
        let file = env
            .call_method(
                activity,
                jni_str!("getCacheDir"),
                jni_sig!("()Ljava/io/File;"),
                &[],
            )?
            .l()?;
        if file.is_null() {
            return Ok(String::new());
        }
        let path = env
            .call_method(
                &file,
                jni_str!("getAbsolutePath"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        to_string(env, path)
    })?;
    if path.is_empty() {
        return Err(ServiceError::Unsupported("getCacheDir returned null"));
    }
    Ok(PathBuf::from(path))
}

/// `startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(url)))`.
pub(super) fn open_url(url: &str) -> Result<(), ServiceError> {
    with_activity("open_url", |env, activity| {
        let jurl = env.new_string(url)?;
        let uri = env
            .call_static_method(
                jni_str!("android/net/Uri"),
                jni_str!("parse"),
                jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                &[JValue::Object(&jurl)],
            )?
            .l()?;
        let action = env.new_string("android.intent.action.VIEW")?;
        let intent = env.new_object(
            jni_str!("android/content/Intent"),
            jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
            &[JValue::Object(&action), JValue::Object(&uri)],
        )?;
        env.call_method(
            activity,
            jni_str!("startActivity"),
            jni_sig!("(Landroid/content/Intent;)V"),
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// `RegiaActivity.regiaSetKeepScreenOn(boolean)`: the generated activity posts the window-flag change
/// to the UI thread. (`AndroidApp::set_window_flags` cannot be used: it takes android-activity's state
/// write lock, which `poll_events` holds for reading while every frame runs, so it deadlocks; ADR-013.)
pub(super) fn set_keep_screen_on(on: bool) -> Result<(), ServiceError> {
    with_activity("keep_screen_on", |env, activity| {
        env.call_method(
            activity,
            jni_str!("regiaSetKeepScreenOn"),
            jni_sig!("(Z)V"),
            &[JValue::Bool(on)],
        )?;
        Ok(())
    })
}

/// `RegiaActivity.regiaMicGranted()`, `regiaMicPrompt()` and `regiaMicCanPrompt()`: whether
/// `RECORD_AUDIO` is held, the prompt state (0 never asked, 1 showing, 2 answered) and whether asking
/// again can show a prompt. Polled once per voice frame while a prompt is on screen.
pub(super) fn microphone_permission() -> Result<(bool, i32, bool), ServiceError> {
    with_activity("microphone_permission", |env, activity| {
        let granted = env
            .call_method(activity, jni_str!("regiaMicGranted"), jni_sig!("()Z"), &[])?
            .z()?;
        let prompt = int_method(env, activity, jni_str!("regiaMicPrompt"))?;
        let can_prompt = env
            .call_method(activity, jni_str!("regiaMicCanPrompt"), jni_sig!("()Z"), &[])?
            .z()?;
        Ok((granted, prompt, can_prompt))
    })
}

/// `RegiaActivity.regiaRequestMic()`: the generated activity shows the prompt on the UI thread and
/// records the answer in `onRequestPermissionsResult`.
pub(super) fn request_microphone_permission() -> Result<(), ServiceError> {
    with_activity("request_microphone_permission", |env, activity| {
        env.call_method(activity, jni_str!("regiaRequestMic"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `Locale.getDefault().toLanguageTag()`.
pub(super) fn default_locale_tag() -> Result<String, ServiceError> {
    with_activity("system_locale", |env, _activity| {
        let locale = env
            .call_static_method(
                jni_str!("java/util/Locale"),
                jni_str!("getDefault"),
                jni_sig!("()Ljava/util/Locale;"),
                &[],
            )?
            .l()?;
        if locale.is_null() {
            return Ok(String::new());
        }
        let tag = env
            .call_method(
                &locale,
                jni_str!("toLanguageTag"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        to_string(env, tag)
    })
}

/// `android.os.Build` facts.
pub(super) struct BuildInfo {
    pub(super) release: String,
    pub(super) sdk_int: i32,
    pub(super) manufacturer: String,
    pub(super) model: String,
}

/// `Build.VERSION.RELEASE`, `Build.VERSION.SDK_INT`, `Build.MANUFACTURER`, `Build.MODEL`.
pub(super) fn build_info() -> Result<BuildInfo, ServiceError> {
    with_activity("os_info", |env, _activity| {
        Ok(BuildInfo {
            release: static_string(env, jni_str!("android/os/Build$VERSION"), jni_str!("RELEASE"))?,
            sdk_int: sdk_int(env)?,
            manufacturer: static_string(env, jni_str!("android/os/Build"), jni_str!("MANUFACTURER"))?,
            model: static_string(env, jni_str!("android/os/Build"), jni_str!("MODEL"))?,
        })
    })
}

fn int_method(env: &mut Env<'_>, obj: &JObject<'_>, name: &JNIStr) -> Result<i32, JniError> {
    env.call_method(obj, name, jni_sig!("()I"), &[])?.i()
}

fn int_field(env: &mut Env<'_>, obj: &JObject<'_>, name: &JNIStr) -> Result<i32, JniError> {
    env.get_field(obj, name, jni_sig!("I"))?.i()
}

/// The root `WindowInsets` of the decor view: `getInsets(systemBars() | displayCutout())` on API 30+,
/// otherwise the system-window insets combined with the display cutout's safe insets (API 28+).
/// `None` before API 23 or while the view is not attached.
pub(super) fn safe_area() -> Result<Option<SafeArea>, ServiceError> {
    with_activity("safe_area", |env, activity| {
        let sdk = sdk_int(env)?;
        if sdk < 23 {
            return Ok(None);
        }
        let window = env
            .call_method(
                activity,
                jni_str!("getWindow"),
                jni_sig!("()Landroid/view/Window;"),
                &[],
            )?
            .l()?;
        if window.is_null() {
            return Ok(None);
        }
        let decor = env
            .call_method(
                &window,
                jni_str!("getDecorView"),
                jni_sig!("()Landroid/view/View;"),
                &[],
            )?
            .l()?;
        if decor.is_null() {
            return Ok(None);
        }
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!("()Landroid/view/WindowInsets;"),
                &[],
            )?
            .l()?;
        if insets.is_null() {
            return Ok(None);
        }
        if sdk >= 30 {
            let type_class = jni_str!("android/view/WindowInsets$Type");
            let bars = env
                .call_static_method(type_class, jni_str!("systemBars"), jni_sig!("()I"), &[])?
                .i()?;
            let cutout = env
                .call_static_method(type_class, jni_str!("displayCutout"), jni_sig!("()I"), &[])?
                .i()?;
            let ins = env
                .call_method(
                    &insets,
                    jni_str!("getInsets"),
                    jni_sig!("(I)Landroid/graphics/Insets;"),
                    &[JValue::Int(bars | cutout)],
                )?
                .l()?;
            if ins.is_null() {
                return Ok(None);
            }
            return Ok(Some(SafeArea::from_insets(
                int_field(env, &ins, jni_str!("left"))?,
                int_field(env, &ins, jni_str!("top"))?,
                int_field(env, &ins, jni_str!("right"))?,
                int_field(env, &ins, jni_str!("bottom"))?,
            )));
        }
        let mut area = SafeArea::from_insets(
            int_method(env, &insets, jni_str!("getSystemWindowInsetLeft"))?,
            int_method(env, &insets, jni_str!("getSystemWindowInsetTop"))?,
            int_method(env, &insets, jni_str!("getSystemWindowInsetRight"))?,
            int_method(env, &insets, jni_str!("getSystemWindowInsetBottom"))?,
        );
        if sdk >= 28 {
            let cutout = env
                .call_method(
                    &insets,
                    jni_str!("getDisplayCutout"),
                    jni_sig!("()Landroid/view/DisplayCutout;"),
                    &[],
                )?
                .l()?;
            if !cutout.is_null() {
                area = area.union(SafeArea::from_insets(
                    int_method(env, &cutout, jni_str!("getSafeInsetLeft"))?,
                    int_method(env, &cutout, jni_str!("getSafeInsetTop"))?,
                    int_method(env, &cutout, jni_str!("getSafeInsetRight"))?,
                    int_method(env, &cutout, jni_str!("getSafeInsetBottom"))?,
                ));
            }
        }
        Ok(Some(area))
    })
}

/// `Context.getSystemService(name)`.
fn system_service<'l>(
    env: &mut Env<'l>,
    activity: &JObject<'_>,
    name: &str,
) -> Result<JObject<'l>, JniError> {
    let jname = env.new_string(name)?;
    env.call_method(
        activity,
        jni_str!("getSystemService"),
        jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
        &[JValue::Object(&jname)],
    )?
    .l()
}

/// One connected input device that reports gamepad or joystick sources
/// (`InputManager.getInputDeviceIds()` + `InputDevice.getName()/getSources()`).
pub(super) struct GamepadDevice {
    pub(super) id: i32,
    pub(super) name: String,
}

/// Constants from `android.view.InputDevice` (public SDK values, see the Android developer docs).
const SOURCE_CLASS_JOYSTICK: i32 = 0x0000_0010;
const SOURCE_GAMEPAD: i32 = 0x0000_0401;

/// Every connected device whose sources include joystick/gamepad classes, with its id and name.
/// Used by the Android gamepad backend for device discovery (`docs/specs/11` § 6).
pub(super) fn gamepad_devices() -> Result<Vec<GamepadDevice>, ServiceError> {
    with_activity("gamepad_devices", |env, activity| {
        let manager = system_service(env, activity, "input")?;
        if manager.is_null() {
            return Ok(Vec::new());
        }
        let ids = env
            .call_method(&manager, jni_str!("getInputDeviceIds"), jni_sig!("()[I"), &[])?
            .l()?;
        if ids.is_null() {
            return Ok(Vec::new());
        }
        let ids = env.cast_local::<jni::objects::JIntArray>(ids)?;
        let len = ids.len(env)?;
        let mut buf = vec![0i32; len];
        if !buf.is_empty() {
            ids.get_region(env, 0, &mut buf)?;
        }
        let mut devices = Vec::new();
        for id in buf {
            let device = env
                .call_method(
                    &manager,
                    jni_str!("getInputDevice"),
                    jni_sig!("(I)Landroid/view/InputDevice;"),
                    &[JValue::Int(id)],
                )?
                .l()?;
            if device.is_null() {
                continue; // the device vanished between calls
            }
            let sources = int_method(env, &device, jni_str!("getSources"))?;
            if sources & SOURCE_CLASS_JOYSTICK == 0 && sources & SOURCE_GAMEPAD != SOURCE_GAMEPAD {
                continue;
            }
            let name = env
                .call_method(
                    &device,
                    jni_str!("getName"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()
                .map(|obj| to_string(env, obj))??;
            devices.push(GamepadDevice { id, name });
        }
        Ok(devices)
    })
}

/// Rumble one input device: `InputDevice.getVibrator().vibrate(VibrationEffect.createOneShot(...))`
/// (API 19+ / 26+; the project's `min_sdk` is 29). Devices without a vibrator report `false`.
pub(super) fn gamepad_rumble(device_id: i32, duration_ms: i64, amplitude: i32) -> Result<bool, ServiceError> {
    with_activity("gamepad_rumble", |env, activity| {
        let manager = system_service(env, activity, "input")?;
        if manager.is_null() {
            return Ok(false);
        }
        let device = env
            .call_method(
                &manager,
                jni_str!("getInputDevice"),
                jni_sig!("(I)Landroid/view/InputDevice;"),
                &[JValue::Int(device_id)],
            )?
            .l()?;
        if device.is_null() {
            return Ok(false);
        }
        let vibrator = env
            .call_method(
                &device,
                jni_str!("getVibrator"),
                jni_sig!("()Landroid/os/Vibrator;"),
                &[],
            )?
            .l()?;
        if vibrator.is_null()
            || !env
                .call_method(&vibrator, jni_str!("hasVibrator"), jni_sig!("()Z"), &[])?
                .z()?
        {
            return Ok(false);
        }
        let effect = env
            .call_static_method(
                jni_str!("android/os/VibrationEffect"),
                jni_str!("createOneShot"),
                jni_sig!("(JI)Landroid/os/VibrationEffect;"),
                &[JValue::Long(duration_ms), JValue::Int(amplitude)],
            )?
            .l()?;
        env.call_method(
            &vibrator,
            jni_str!("vibrate"),
            jni_sig!("(Landroid/os/VibrationEffect;)V"),
            &[JValue::Object(&effect)],
        )?;
        Ok(true)
    })
}

/// One-shot vibration: `VibratorManager.getDefaultVibrator()` on API 31+, the `vibrator` service
/// below; `VibrationEffect.createOneShot` on API 26+, `vibrate(long)` below. Devices without a
/// vibrator do nothing.
pub(super) fn vibrate(duration_ms: i64, amplitude: i32) -> Result<(), ServiceError> {
    let vibrated = with_activity("vibrate", |env, activity| {
        let sdk = sdk_int(env)?;
        let vibrator = if sdk >= 31 {
            let manager = system_service(env, activity, "vibrator_manager")?;
            if manager.is_null() {
                return Ok(false);
            }
            env.call_method(
                &manager,
                jni_str!("getDefaultVibrator"),
                jni_sig!("()Landroid/os/Vibrator;"),
                &[],
            )?
            .l()?
        } else {
            system_service(env, activity, "vibrator")?
        };
        if vibrator.is_null()
            || !env
                .call_method(&vibrator, jni_str!("hasVibrator"), jni_sig!("()Z"), &[])?
                .z()?
        {
            return Ok(false);
        }
        if sdk >= 26 {
            let effect = env
                .call_static_method(
                    jni_str!("android/os/VibrationEffect"),
                    jni_str!("createOneShot"),
                    jni_sig!("(JI)Landroid/os/VibrationEffect;"),
                    &[JValue::Long(duration_ms), JValue::Int(amplitude)],
                )?
                .l()?;
            env.call_method(
                &vibrator,
                jni_str!("vibrate"),
                jni_sig!("(Landroid/os/VibrationEffect;)V"),
                &[JValue::Object(&effect)],
            )?;
        } else {
            env.call_method(
                &vibrator,
                jni_str!("vibrate"),
                jni_sig!("(J)V"),
                &[JValue::Long(duration_ms)],
            )?;
        }
        Ok(true)
    })?;
    if !vibrated {
        tracing::debug!(target: "regia_platform", "vibrate: no vibrator on this device");
    }
    Ok(())
}

/// `PowerManager.getCurrentThermalStatus()` (API 29+) and `getThermalHeadroom(10)` (API 30+).
pub(super) fn thermal() -> Result<crate::services::ThermalState, ServiceError> {
    with_activity("thermal", |env, activity| {
        let power = system_service(env, activity, "power")?;
        if power.is_null() {
            return Ok(crate::services::ThermalState::default());
        }
        let status = int_method(env, &power, jni_str!("getCurrentThermalStatus"))?;
        let headroom = if sdk_int(env)? >= 30 {
            let raw = env
                .call_method(
                    &power,
                    jni_str!("getThermalHeadroom"),
                    jni_sig!("(I)F"),
                    &[JValue::Int(10)],
                )?
                .f()?;
            crate::services::thermal::sane_headroom(raw)
        } else {
            None
        };
        Ok(crate::services::ThermalState {
            status: crate::services::ThermalStatus::from_android(status),
            headroom,
        })
    })
}

/// `ConnectivityManager.isActiveNetworkMetered()` (needs `ACCESS_NETWORK_STATE`, a normal
/// permission `regia package` always declares): `None` when there is no active network
/// (`getActiveNetwork()` is null) or no connectivity service.
pub(super) fn active_network_metered() -> Result<Option<bool>, ServiceError> {
    with_activity("network_metered", |env, activity| {
        let manager = system_service(env, activity, "connectivity")?;
        if manager.is_null() {
            return Ok(None);
        }
        let network = env
            .call_method(
                &manager,
                jni_str!("getActiveNetwork"),
                jni_sig!("()Landroid/net/Network;"),
                &[],
            )?
            .l()?;
        if network.is_null() {
            return Ok(None);
        }
        let metered = env
            .call_method(&manager, jni_str!("isActiveNetworkMetered"), jni_sig!("()Z"), &[])?
            .z()?;
        Ok(Some(metered))
    })
}

/// `CaptioningManager.isEnabled()`: the user's captions setting.
pub(super) fn captioning_enabled() -> Result<bool, ServiceError> {
    with_activity("system_captions", |env, activity| {
        let manager = system_service(env, activity, "captioning")?;
        if manager.is_null() {
            return Ok(false);
        }
        env.call_method(&manager, jni_str!("isEnabled"), jni_sig!("()Z"), &[])?
            .z()
    })
}

/// `getResources().getConfiguration().fontScale`: the user's font size setting.
pub(super) fn font_scale() -> Result<f32, ServiceError> {
    with_activity("system_font_scale", |env, activity| {
        let resources = env
            .call_method(
                activity,
                jni_str!("getResources"),
                jni_sig!("()Landroid/content/res/Resources;"),
                &[],
            )?
            .l()?;
        let config = env
            .call_method(
                &resources,
                jni_str!("getConfiguration"),
                jni_sig!("()Landroid/content/res/Configuration;"),
                &[],
            )?
            .l()?;
        env.get_field(&config, jni_str!("fontScale"), jni_sig!("F"))?.f()
    })
}

/// `getResources().getConfiguration().densityDpi`: the display density with the user's Display size
/// setting (and `wm density`) applied (ADR-5379).
pub(super) fn density_dpi() -> Result<i32, ServiceError> {
    with_activity("system_display_scale", |env, activity| {
        let resources = env
            .call_method(
                activity,
                jni_str!("getResources"),
                jni_sig!("()Landroid/content/res/Resources;"),
                &[],
            )?
            .l()?;
        let config = env
            .call_method(
                &resources,
                jni_str!("getConfiguration"),
                jni_sig!("()Landroid/content/res/Configuration;"),
                &[],
            )?
            .l()?;
        env.get_field(&config, jni_str!("densityDpi"), jni_sig!("I"))?.i()
    })
}

/// `RegiaActivity.regiaState()`: bit 0 = `onPause` ran, bit 1 = `onDestroy` ran.
pub(super) fn activity_flags() -> Result<i32, ServiceError> {
    with_activity("activity_flags", |env, activity| {
        int_method(env, activity, jni_str!("regiaState"))
    })
}

/// One `ApplicationExitInfo` of an earlier run of this app.
pub(super) struct PastExit {
    pub(super) reason: i32,
    pub(super) status: i32,
    pub(super) timestamp_ms: i64,
    pub(super) description: String,
}

/// `ActivityManager.getHistoricalProcessExitReasons(null, 0, max)` (API 30+): why this app's
/// recent earlier processes ended (native crash, ANR, low-memory kill, ...). Newest first.
pub(super) fn past_exits(max: i32) -> Result<Vec<PastExit>, ServiceError> {
    with_activity("past_exits", |env, activity| {
        if sdk_int(env)? < 30 {
            return Ok(Vec::new());
        }
        let manager = system_service(env, activity, "activity")?;
        if manager.is_null() {
            return Ok(Vec::new());
        }
        let none = JObject::null();
        let list = env
            .call_method(
                &manager,
                jni_str!("getHistoricalProcessExitReasons"),
                jni_sig!("(Ljava/lang/String;II)Ljava/util/List;"),
                &[JValue::Object(&none), JValue::Int(0), JValue::Int(max)],
            )?
            .l()?;
        if list.is_null() {
            return Ok(Vec::new());
        }
        let len = int_method(env, &list, jni_str!("size"))?;
        let mut exits = Vec::new();
        for i in 0..len {
            let info = env
                .call_method(
                    &list,
                    jni_str!("get"),
                    jni_sig!("(I)Ljava/lang/Object;"),
                    &[JValue::Int(i)],
                )?
                .l()?;
            if info.is_null() {
                continue;
            }
            let description = env
                .call_method(
                    &info,
                    jni_str!("getDescription"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()
                .map(|obj| to_string(env, obj))??;
            exits.push(PastExit {
                reason: int_method(env, &info, jni_str!("getReason"))?,
                status: int_method(env, &info, jni_str!("getStatus"))?,
                timestamp_ms: env
                    .call_method(&info, jni_str!("getTimestamp"), jni_sig!("()J"), &[])?
                    .j()?,
                description,
            });
        }
        Ok(exits)
    })
}

/// `RegiaActivity.regiaPackState(String)`: `{status, errorCode, bytesDownloaded, totalBytesToDownload}`
/// or null before the store answered (specs/40 § 2.6). The method exists only in builds with asset
/// packs; without it the call fails and the caller treats the store as absent.
pub(super) fn pack_state(pack: &str) -> Result<Option<crate::asset_packs::PackState>, ServiceError> {
    with_activity("asset_pack_state", |env, activity| {
        let name = env.new_string(pack)?;
        let row = env
            .call_method(
                activity,
                jni_str!("regiaPackState"),
                jni_sig!("(Ljava/lang/String;)[J"),
                &[JValue::Object(&name)],
            )?
            .l()?;
        if row.is_null() {
            return Ok(None);
        }
        let row = env.cast_local::<jni::objects::JLongArray>(row)?;
        let len = row.len(env)?;
        let mut buf = vec![0i64; len];
        if !buf.is_empty() {
            row.get_region(env, 0, &mut buf)?;
        }
        Ok(crate::asset_packs::PackState::from_row(&buf))
    })
}

/// `regiaPackFetch(String)`, `regiaPackCancel(String)`, `regiaPackRemove(String)` or
/// `regiaPackConfirm()` on the generated activity.
pub(super) fn pack_call(pack: &str, what: crate::asset_packs::PackCall) -> Result<(), ServiceError> {
    use crate::asset_packs::PackCall;
    with_activity("asset_pack_call", |env, activity| {
        if what == PackCall::Confirm {
            env.call_method(activity, jni_str!("regiaPackConfirm"), jni_sig!("()V"), &[])?;
            return Ok(());
        }
        let name = env.new_string(pack)?;
        let method = match what {
            PackCall::Fetch => jni_str!("regiaPackFetch"),
            PackCall::Cancel => jni_str!("regiaPackCancel"),
            PackCall::Remove | PackCall::Confirm => jni_str!("regiaPackRemove"),
        };
        env.call_method(
            activity,
            method,
            jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&name)],
        )?;
        Ok(())
    })
}

/// `regiaPackLocation(String)`: the pack's extracted-files directory, or null.
pub(super) fn pack_location(pack: &str) -> Result<Option<PathBuf>, ServiceError> {
    let path = with_activity("asset_pack_location", |env, activity| {
        let name = env.new_string(pack)?;
        let path = env
            .call_method(
                activity,
                jni_str!("regiaPackLocation"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&name)],
            )?
            .l()?;
        to_string(env, path)
    })?;
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

fn extra_int(env: &mut Env<'_>, intent: &JObject<'_>, key: &str, default: i32) -> Result<i32, JniError> {
    let name = env.new_string(key)?;
    env.call_method(
        intent,
        jni_str!("getIntExtra"),
        jni_sig!("(Ljava/lang/String;I)I"),
        &[JValue::Object(&name), JValue::Int(default)],
    )?
    .i()
}

fn extra_bool(env: &mut Env<'_>, intent: &JObject<'_>, key: &str, default: bool) -> Result<bool, JniError> {
    let name = env.new_string(key)?;
    env.call_method(
        intent,
        jni_str!("getBooleanExtra"),
        jni_sig!("(Ljava/lang/String;Z)Z"),
        &[JValue::Object(&name), JValue::Bool(default)],
    )?
    .z()
}

fn battery_extra(env: &mut Env<'_>, field: &JNIStr) -> Result<String, JniError> {
    static_string(env, jni_str!("android/os/BatteryManager"), field)
}

/// Sticky `Intent.ACTION_BATTERY_CHANGED` via `registerReceiver(null, IntentFilter)`.
///
/// The extra keys are the `BatteryManager` static strings (`EXTRA_PRESENT`, `EXTRA_PLUGGED`,
/// `EXTRA_LEVEL`, `EXTRA_SCALE`, `EXTRA_STATUS`). `EXTRA_STATUS` is read and does not change
/// the result. A null sticky intent is [`crate::services::PowerState::unknown`].
pub(super) fn battery_power() -> Result<crate::services::PowerState, ServiceError> {
    with_activity("power_state", |env, activity| {
        let action = static_string(
            env,
            jni_str!("android/content/Intent"),
            jni_str!("ACTION_BATTERY_CHANGED"),
        )?;
        let action = env.new_string(action)?;
        let filter = env.new_object(
            jni_str!("android/content/IntentFilter"),
            jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&action)],
        )?;
        let receiver = JObject::null();
        let sticky = env
            .call_method(
                activity,
                jni_str!("registerReceiver"),
                jni_sig!(
                    "(Landroid/content/BroadcastReceiver;Landroid/content/IntentFilter;)Landroid/content/Intent;"
                ),
                &[JValue::Object(&receiver), JValue::Object(&filter)],
            )?
            .l()?;
        if sticky.is_null() {
            return Ok(crate::services::PowerState::unknown());
        }
        let present_key = battery_extra(env, jni_str!("EXTRA_PRESENT"))?;
        let plugged_key = battery_extra(env, jni_str!("EXTRA_PLUGGED"))?;
        let level_key = battery_extra(env, jni_str!("EXTRA_LEVEL"))?;
        let scale_key = battery_extra(env, jni_str!("EXTRA_SCALE"))?;
        let status_key = battery_extra(env, jni_str!("EXTRA_STATUS"))?;
        let present = extra_bool(env, &sticky, &present_key, false)?;
        let plugged = extra_int(env, &sticky, &plugged_key, 0)?;
        let level = extra_int(env, &sticky, &level_key, -1)?;
        let scale = extra_int(env, &sticky, &scale_key, -1)?;
        let _status = extra_int(env, &sticky, &status_key, 1)?;
        Ok(crate::services::power::from_android_battery(
            present, plugged, level, scale,
        ))
    })
}

/// `getIntent().getStringExtra(key)` of the activity: the string extra the launching intent
/// carried under `key` (`am start --es <key> <value>`), `None` when it has none (ADR-8472).
pub(super) fn intent_string_extra(key: &str) -> Result<Option<String>, ServiceError> {
    with_activity("getIntent", |env, activity| {
        let intent = env
            .call_method(
                activity,
                jni_str!("getIntent"),
                jni_sig!("()Landroid/content/Intent;"),
                &[],
            )?
            .l()?;
        if intent.is_null() {
            return Ok(None);
        }
        let jkey = env.new_string(key)?;
        let value = env
            .call_method(
                &intent,
                jni_str!("getStringExtra"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&jkey)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        to_string(env, value).map(Some)
    })
}
