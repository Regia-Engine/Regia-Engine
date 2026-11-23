// SPDX-License-Identifier: MPL-2.0
//! The clipboard seam of the runtime UI (ADR-336). Text widgets never call a platform clipboard
//! directly: every copy, cut and paste goes through the [`UiClipboard`] resource, which holds one
//! [`ClipboardProvider`].
//!
//! The default provider, [`SystemClipboard`], uses `regia_platform`'s clipboard service (the
//! system clipboard on Linux, Windows and macOS, opened on first use so headless runs never touch
//! it) and keeps an app-local copy as a fallback: where the platform has no clipboard (Android
//! today, or a desktop without a display) copy and paste still work inside the app. Tests and hosts
//! with their own clipboard insert a different provider, e.g. [`MemoryClipboard`].

use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;

/// A text clipboard the UI can read and write.
pub trait ClipboardProvider: Send + Sync + 'static {
    /// The clipboard's text, or `None` when it is empty, holds no text or cannot be read.
    fn get_text(&mut self) -> Option<String>;
    /// Replaces the clipboard's content with `text`.
    fn set_text(&mut self, text: &str);
}

/// The clipboard the runtime UI uses. Insert one built with [`UiClipboard::new`] to replace the
/// default [`SystemClipboard`].
#[derive(Resource)]
pub struct UiClipboard {
    provider: Box<dyn ClipboardProvider>,
}

impl UiClipboard {
    /// A UI clipboard backed by `provider`.
    pub fn new(provider: impl ClipboardProvider) -> Self {
        Self {
            provider: Box::new(provider),
        }
    }

    /// The clipboard's text, if any.
    pub fn get_text(&mut self) -> Option<String> {
        self.provider.get_text()
    }

    /// Replaces the clipboard's content with `text`.
    pub fn set_text(&mut self, text: &str) {
        self.provider.set_text(text);
    }
}

impl Default for UiClipboard {
    fn default() -> Self {
        Self::new(SystemClipboard::default())
    }
}

/// The platform clipboard with an app-local fallback (see the module docs).
#[derive(Default)]
pub struct SystemClipboard {
    platform: regia_platform::Clipboard,
    local: Option<String>,
}

impl ClipboardProvider for SystemClipboard {
    /// The platform clipboard's text. An empty read is `None`. A read error is `None`,
    /// except [`regia_platform::ServiceError::Unsupported`] (the platform has no clipboard),
    /// which returns the app-local copy. A failed read of a real clipboard does not return
    /// the last string this app wrote.
    fn get_text(&mut self) -> Option<String> {
        let read = self.platform.get_text();
        if let Err(e) = &read {
            if matches!(e, regia_platform::ServiceError::Unsupported(_)) {
                tracing::debug!(target: "regia_ui", "clipboard read used the app-local copy: {e}");
            } else {
                tracing::debug!(target: "regia_ui", "clipboard read failed: {e}");
            }
        }
        text_after_platform_read(read, &self.local)
    }

    fn set_text(&mut self, text: &str) {
        self.local = Some(text.to_owned());
        if let Err(e) = self.platform.set_text(text) {
            tracing::debug!(target: "regia_ui", "clipboard write kept app-local only: {e}");
        }
    }
}

/// What [`SystemClipboard::get_text`] returns for one platform read.
///
/// The app-local copy is only the fallback where the platform has no clipboard
/// ([`regia_platform::ServiceError::Unsupported`]). Any other error, and an empty string,
/// are `None` (ADR-336: the copy is not a cache of the last successful write).
fn text_after_platform_read(
    read: Result<String, regia_platform::ServiceError>,
    local: &Option<String>,
) -> Option<String> {
    match read {
        Ok(text) if !text.is_empty() => Some(text),
        Ok(_) => None,
        Err(regia_platform::ServiceError::Unsupported(_)) => local.clone().filter(|text| !text.is_empty()),
        Err(_) => None,
    }
}

/// An in-memory clipboard. Clones share one buffer, so a test can keep a handle and inspect
/// what the UI copied.
#[derive(Debug, Clone, Default)]
pub struct MemoryClipboard {
    text: Arc<Mutex<Option<String>>>,
}

impl MemoryClipboard {
    /// The current content.
    pub fn contents(&self) -> Option<String> {
        self.text.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl ClipboardProvider for MemoryClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.contents()
    }

    fn set_text(&mut self, text: &str) {
        *self.text.lock().unwrap_or_else(PoisonError::into_inner) = Some(text.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_clipboard_round_trips_and_shares_its_buffer() {
        let probe = MemoryClipboard::default();
        let mut clipboard = UiClipboard::new(probe.clone());
        assert_eq!(clipboard.get_text(), None);
        clipboard.set_text("héllo");
        assert_eq!(clipboard.get_text().as_deref(), Some("héllo"));
        assert_eq!(
            probe.contents().as_deref(),
            Some("héllo"),
            "clones share the buffer"
        );
    }

    #[test]
    fn system_clipboard_read_error_does_not_return_stale_local() {
        let local = Some("secret".to_owned());
        let os = regia_platform::ServiceError::Os {
            operation: "clipboard get_text",
            message: "clipboard holds an image".to_owned(),
        };
        assert_eq!(
            text_after_platform_read(Err(os), &local),
            None,
            "a failed read of a real clipboard must not return the last in-app string"
        );
        let unsupported = regia_platform::ServiceError::Unsupported("clipboard unavailable");
        assert_eq!(
            text_after_platform_read(Err(unsupported), &local).as_deref(),
            Some("secret"),
            "no clipboard still pastes the app-local copy"
        );
        assert_eq!(
            text_after_platform_read(Ok(String::new()), &local),
            None,
            "an empty platform read is None, not the stale local string"
        );
        assert_eq!(
            text_after_platform_read(Ok("other".to_owned()), &local).as_deref(),
            Some("other")
        );
        let empty_local = Some(String::new());
        let unsupported = regia_platform::ServiceError::Unsupported("Android clipboard is Tier 2");
        assert_eq!(text_after_platform_read(Err(unsupported), &empty_local), None);
    }
}
