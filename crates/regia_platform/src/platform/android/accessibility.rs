// SPDX-License-Identifier: MPL-2.0
//! TalkBack bridge (ADR-3454): one `accesskit_android::InjectingAdapter` on GameActivity's
//! `mSurfaceView`.
//!
//! The view lookup follows AccessKit's published winit adapter (Apache-2.0, read as a design
//! reference; not a dependency). JNI goes through `accesskit_android`'s own `jni` 0.21. This module
//! never calls `attach_current_thread`: `android_main` is already attached by the workspace `jni`
//! 0.22, and a 0.21 attach guard would detach that thread when it dropped.
//!
//! A failed lookup returns `Err` and the runner records it. `InjectingAdapter::new` and
//! `update_if_active` still unwrap internally; those panics are caught here in dev and release
//! (`panic = "abort"` is the shipping profile only, so a shipping build still ends if one fires
//! after the preflight). A caught update panic forgets the adapter so its `Drop` cannot unwrap, and
//! disables the bridge so later frames queue nothing.

use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};

use accesskit_android::InjectingAdapter;
use accesskit_android::jni::JavaVM;
use accesskit_android::jni::objects::{GlobalRef, JObject};

use crate::accessibility::{
    AccessKitBridge, AndroidAttachBlock, AndroidUpdateGate, BridgeHandler, OsAccessKitAdapter,
    android_attach_decision, android_update_gate, fold_descriptions_into_labels,
};

/// The adapter's name in the agent's report.
pub(crate) const NAME: &str = "android";
/// Android does not subclass the window before show, so the window is created visible.
pub(crate) const CREATE_HIDDEN: bool = false;

/// GameActivity's private surface field. JNI `GetField` reads it on the superclass.
const SURFACE_VIEW_FIELD: &str = "mSurfaceView";
const SURFACE_VIEW_SIG: &str = "Lcom/google/androidgamesdk/GameActivity$InputEnabledSurfaceView;";
const DELEGATE_SIG: &str = "()Landroid/view/View$AccessibilityDelegate;";

/// The live adapter, or `None` after an update panic (the value was forgotten, not dropped).
struct AndroidAdapter {
    inner: Option<InjectingAdapter>,
    bridge: AccessKitBridge,
    /// The process VM, for the accessibility check on the thread that updates.
    vm: JavaVM,
    /// `Context.getSystemService("accessibility")`; `None` when the lookup failed (then every
    /// update is sent, as before the check existed).
    manager: Option<GlobalRef>,
    /// An update was held while accessibility was off ([`android_update_gate`]).
    held: bool,
}

impl AndroidAdapter {
    /// `AccessibilityManager.isEnabled()`. A failed call counts as on, and its Java exception is
    /// cleared so it cannot surface in a later JNI call.
    fn accessibility_on(&self) -> bool {
        let Some(manager) = &self.manager else {
            return true;
        };
        let Ok(mut env) = self.vm.get_env() else {
            return true;
        };
        match env
            .call_method(manager.as_obj(), "isEnabled", "()Z", &[])
            .and_then(|v| v.z())
        {
            Ok(on) => on,
            Err(_) => {
                let _ = env.exception_clear();
                true
            }
        }
    }
}

impl OsAccessKitAdapter for AndroidAdapter {
    fn update(&mut self, factory: Box<dyn FnOnce() -> accesskit::TreeUpdate + '_>) {
        // Android refuses accessibility events while accessibility is off, and the adapter would
        // abort the game on that refusal (ADR-4932): hold updates then, resync when it is back.
        match android_update_gate(self.accessibility_on(), &mut self.held) {
            AndroidUpdateGate::Hold => return,
            AndroidUpdateGate::Resync => self.bridge.request_resync(),
            AndroidUpdateGate::Send => {}
        }
        // TalkBack reads only the label through this adapter: fold the description in (ADR-4932).
        let folded = || fold_descriptions_into_labels(factory());
        let panicked = self
            .inner
            .as_mut()
            .is_some_and(|inner| catch_unwind(AssertUnwindSafe(|| inner.update_if_active(folded))).is_err());
        if !panicked {
            return;
        }
        tracing::warn!(
            target: "regia_platform",
            "screen-reader bridge: Android update panicked; the bridge is off for this process"
        );
        self.bridge
            .disable_adapter("InjectingAdapter::update_if_active panicked; the bridge is off");
        if let Some(dead) = self.inner.take() {
            // Drop posts to the UI thread and unwraps. Forgetting it leaves the Java delegate in
            // place and skips that path.
            std::mem::forget(dead);
        }
    }

    fn set_window_focused(&mut self, _focused: bool) {
        // AccessKit's Android adapter has no window-focus call. TalkBack uses the view's own focus.
    }
}

/// Creates the adapter for the activity's surface view. `_window` is unused: the view is a Java
/// field of the activity, not a winit handle.
pub(crate) fn create(
    _window: &winit::window::Window,
    bridge: &AccessKitBridge,
) -> Result<Box<dyn OsAccessKitAdapter>, String> {
    let Some(app) = super::app() else {
        return Err(AndroidAttachBlock::NoActivity.reason().to_owned());
    };
    let adapter = inject(&app, bridge)?;
    // SAFETY: as in `inject`: the process JavaVM, not owned, never destroyed here.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }
        .map_err(|err| format!("JavaVM::from_raw: {err}"))?;
    let manager = accessibility_manager(&app, &vm);
    if manager.is_none() {
        tracing::warn!(
            target: "regia_platform",
            "screen-reader bridge: no AccessibilityManager; updates are not held while accessibility is off"
        );
    }
    Ok(Box::new(AndroidAdapter {
        inner: Some(adapter),
        bridge: bridge.clone(),
        vm,
        manager,
        held: false,
    }))
}

/// The activity's `AccessibilityManager` as a global reference, or `None` (any Java exception is
/// cleared).
fn accessibility_manager(app: &super::AndroidApp, vm: &JavaVM) -> Option<GlobalRef> {
    let mut env = vm.get_env().ok()?;
    let activity_ptr = app.activity_as_ptr();
    if activity_ptr.is_null() {
        return None;
    }
    // SAFETY: as in `inject`: an unowned JNI global that outlives this call; not deleted here.
    let activity = ManuallyDrop::new(unsafe { JObject::from_raw(activity_ptr.cast()) });
    let found = (|| {
        let name = env.new_string("accessibility")?;
        let manager = env
            .call_method(
                &*activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&name).into()],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(None);
        }
        env.new_global_ref(manager).map(Some)
    })();
    match found {
        Ok(manager) => manager,
        Err(_) => {
            let _ = env.exception_clear();
            None
        }
    }
}

fn inject(app: &super::AndroidApp, bridge: &AccessKitBridge) -> Result<InjectingAdapter, String> {
    // SAFETY: `vm_as_ptr` is the process JavaVM for as long as this `AndroidApp` lives.
    // `JavaVM::from_raw` does not take ownership and does not destroy the VM on drop.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }
        .map_err(|err| format!("JavaVM::from_raw: {err}"))?;
    // `get_env` only. A detached thread is a hard stop: attaching with jni 0.21 would detach the
    // thread the rest of the engine already uses.
    let mut env = vm
        .get_env()
        .map_err(|err| format!("JNI thread is not attached ({err}); the Android adapter is off"))?;

    let activity_ptr = app.activity_as_ptr();
    if activity_ptr.is_null() {
        return Err(AndroidAttachBlock::NoActivity.reason().to_owned());
    }
    // SAFETY: `activity_as_ptr` is an unowned JNI global owned by android-activity, valid until
    // `app` is dropped. `ManuallyDrop` keeps Drop from deleting a reference this crate does not own.
    let activity = ManuallyDrop::new(unsafe { JObject::from_raw(activity_ptr.cast()) });

    let view = match env.get_field(&*activity, SURFACE_VIEW_FIELD, SURFACE_VIEW_SIG) {
        Ok(value) => value
            .l()
            .map_err(|err| format!("{} ({err})", AndroidAttachBlock::NoSurfaceView.reason()))?,
        Err(err) => {
            return Err(format!("{} ({err})", AndroidAttachBlock::NoSurfaceView.reason()));
        }
    };
    // A null view fails `call_method` with a generic null-pointer error. Name it before that call.
    if view.is_null() {
        return Err(AndroidAttachBlock::NoSurfaceView.reason().to_owned());
    }
    let delegate_set = match env.call_method(&view, "getAccessibilityDelegate", DELEGATE_SIG, &[]) {
        Ok(value) => {
            let delegate = value
                .l()
                .map_err(|err| format!("getAccessibilityDelegate: {err}"))?;
            !delegate.is_null()
        }
        Err(err) => return Err(format!("getAccessibilityDelegate: {err}")),
    };
    android_attach_decision(true, !view.is_null(), delegate_set)
        .map_err(|block| block.reason().to_owned())?;

    let activation = BridgeHandler::new(bridge.clone());
    let action = BridgeHandler::new(bridge.clone());
    // `new` posts the delegate to the UI thread and unwraps the JNI calls that do it. A panic here
    // is a failed class load or a JNI exception, not a logic bug. The "delegate already set" panic
    // lives in that posted callback, which is why the preflight above refuses first.
    match catch_unwind(AssertUnwindSafe(|| {
        InjectingAdapter::new(&mut env, &view, activation, action)
    })) {
        Ok(adapter) => Ok(adapter),
        Err(_) => Err("InjectingAdapter::new panicked (JNI); the bridge stays off".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{CREATE_HIDDEN, NAME};

    #[test]
    fn the_android_build_registers_the_adapter() {
        assert_eq!(NAME, "android");
        assert!(!CREATE_HIDDEN);
    }
}
