// SPDX-License-Identifier: MPL-2.0
//! The hook through which [`AssetServer`] reaches content loaders registered above it
//! (`specs/57` § 4, ADR-2844).
//!
//! `regia_asset` sits below the extension registry, so it cannot name an extension loader. It
//! defines this trait; the framework's `ExtensionPlugin` implements it over the loaders of the
//! active extensions and installs it with [`AssetServer::set_content_hook`]. After that,
//! [`AssetServer::load_content`] and [`AssetServer::load_content_any`] decode cooked content
//! (paths such as `ext://chatter/hello.dlg`) with whichever loader claims the file extension, the
//! same way the typed mesh, texture and material loads decode their own kinds.
//!
//! The decoded value crosses the crate boundary as `Box<dyn Any + Send>` because this crate has
//! no reflection dependency; the framework's hook returns a boxed `Box<dyn Reflect>`.

use std::any::Any;

use crate::loader::AssetLoadError;
use crate::server::AssetServer;

/// Decodes content that a crate above the asset server owns.
pub trait ContentLoaderHook: Send + Sync + 'static {
    /// Whether a loader claims files with this extension (no dot, any case).
    fn handles(&self, file_extension: &str) -> bool;

    /// Decodes `bytes` (the cooked payload of `path`).
    ///
    /// # Errors
    ///
    /// A message for malformed input or an extension that is no longer active.
    fn load(&self, file_extension: &str, bytes: &[u8], path: &str) -> Result<Box<dyn Any + Send>, String>;
}

fn file_extension_of(path: &str) -> &str {
    match path.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.contains(['/', '\\']) => ext,
        _ => "",
    }
}

impl AssetServer {
    /// Installs the hook [`Self::load_content`] decodes with; replaces any earlier one.
    pub fn set_content_hook(&mut self, hook: Option<std::sync::Arc<dyn ContentLoaderHook>>) {
        self.content_hook = hook;
    }

    /// Whether a content hook is installed.
    #[must_use]
    pub fn has_content_hook(&self) -> bool {
        self.content_hook.is_some()
    }

    /// Whether a hook is installed and claims `path`'s file extension.
    #[must_use]
    pub fn has_content_loader(&self, path: &str) -> bool {
        self.content_hook
            .as_ref()
            .is_some_and(|hook| hook.handles(file_extension_of(path)))
    }

    /// Decodes the cooked content at `path` with the installed hook.
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::Invalid`] when no hook is installed or none claims the file extension,
    /// the path is not cooked, or the loader refuses the bytes; the cooked-file and IO errors of
    /// [`Self::read_cooked`] otherwise.
    pub fn load_content_any(&mut self, path: &str) -> Result<Box<dyn Any + Send>, AssetLoadError> {
        let _span = tracing::info_span!("asset_load_content", path).entered();
        let extension = file_extension_of(path);
        let hook = self
            .content_hook
            .clone()
            .filter(|hook| hook.handles(extension))
            .ok_or_else(|| {
                AssetLoadError::Invalid(format!("no content loader handles `.{extension}` files"))
            })?;
        let id = self
            .id_for_path(path)
            .ok_or_else(|| AssetLoadError::Invalid(format!("no cooked content at `{path}`")))?;
        let (bytes, _) = self.read_cooked(id)?;
        let (_, payload) = crate::cooked_header::decode_cooked_file(&bytes)?;
        hook.load(extension, payload, path)
            .map_err(AssetLoadError::Invalid)
    }

    /// [`Self::load_content_any`], downcast to `T` (for the framework's loaders, `T` is
    /// `Box<dyn Reflect>`).
    ///
    /// # Errors
    ///
    /// As [`Self::load_content_any`], and [`AssetLoadError::Invalid`] when the loader produced
    /// another type.
    pub fn load_content<T: Any + Send>(&mut self, path: &str) -> Result<T, AssetLoadError> {
        self.load_content_any(path)?
            .downcast::<T>()
            .map(|boxed| *boxed)
            .map_err(|_| AssetLoadError::Invalid(format!("the loader for `{path}` produced another type")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_extension_is_the_last_dotted_part_of_the_file_name() {
        assert_eq!(file_extension_of("ext://chatter/hello.dlg"), "dlg");
        assert_eq!(file_extension_of("a/b.c/noext"), "");
        assert_eq!(file_extension_of(".hidden"), "");
        assert_eq!(file_extension_of("x.tar.gz"), "gz");
        assert_eq!(file_extension_of(""), "");
    }
}
