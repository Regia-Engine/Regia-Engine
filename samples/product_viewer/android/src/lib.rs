// SPDX-License-Identifier: MIT-0
//! Android entry point of the product viewer. It does not use `regia::android_main!`: the viewer
//! links the platform layer and its own small renderer, not the whole engine.

/// Called by `android-activity` on the native thread GameActivity starts.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
// SAFETY: `android-activity` calls this one symbol, with this signature. A second definition in the
// same library is a link error, not a silent replacement (ADR-012 § 5; the same reason
// `unsafe_code` is allowed on this item in `regia::android_main!`).
#[unsafe(no_mangle)]
fn android_main(app: product_viewer::AndroidApp) {
    product_viewer::run_android(app);
}
