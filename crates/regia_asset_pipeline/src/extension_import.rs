// SPDX-License-Identifier: MPL-2.0
//! Importers extensions register (`docs/specs/57-plugin-extension-api.md` § 3.3.2).
//!
//! An [`Importer`] turns a source file into files of the project's import folder. The built-in
//! importers run first; [`ImporterRegistry::for_file`] answers for file extensions they do not
//! take. Output formats an extension invents carry their own `format_version`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Why an extension importer failed: a message for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("import failed: {0}")]
pub struct ExtensionImportError(pub String);

/// What one import produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOutput {
    /// `(file name relative to the import folder, bytes)`.
    pub files: Vec<(String, Vec<u8>)>,
    /// Other source files the output depends on.
    pub dependencies: Vec<String>,
}

/// An asset-pipeline importer for file extensions the engine does not take.
pub trait Importer: Send + Sync + 'static {
    /// File extensions without the dot, e.g. `["dlg"]`.
    fn file_extensions(&self) -> &[&str];

    /// Imports `input`.
    ///
    /// # Errors
    ///
    /// [`ExtensionImportError`]; the host writes nothing for the file.
    fn import(&self, input: &Path, out: &mut ImportOutput) -> Result<(), ExtensionImportError>;
}

/// Importers by lowercase file extension, with the registering extension.
#[derive(Clone, Default)]
pub struct ImporterRegistry {
    by_extension: BTreeMap<String, (String, Arc<dyn Importer>)>,
}

impl ImporterRegistry {
    /// Maps `file_extension` to `importer`, owned by extension `owner`. An extension another
    /// owner already holds stays with it (logged): replacing it would leave the extension unmapped
    /// when the replacing owner is removed while the first is still enabled. The owner itself may
    /// register again to replace its own importer.
    pub fn insert(&mut self, file_extension: &str, owner: &str, importer: Arc<dyn Importer>) {
        let key = file_extension.to_ascii_lowercase();
        if let Some((holder, _)) = self.by_extension.get(&key)
            && holder != owner
        {
            tracing::warn!(target: "regia_asset_pipeline", extension = %key, holder = %holder, owner, "file extension already has an importer; keeping the first");
            return;
        }
        self.by_extension.insert(key, (owner.to_owned(), importer));
    }

    /// Drops the entries of extension `owner`.
    pub fn remove_owner(&mut self, owner: &str) {
        self.by_extension.retain(|_, (o, _)| o != owner);
    }

    /// The registered file extensions, sorted.
    pub fn file_extensions(&self) -> impl Iterator<Item = &str> {
        self.by_extension.keys().map(String::as_str)
    }

    /// Takes the entries of extension `owner` out, for [`Self::restore`] if a reload fails.
    pub fn take_owner(&mut self, owner: &str) -> Self {
        let (mine, rest) = std::mem::take(&mut self.by_extension)
            .into_iter()
            .partition(|(_, (o, _))| o == owner);
        self.by_extension = rest;
        Self { by_extension: mine }
    }

    /// Puts back what [`Self::take_owner`] took, over anything the failed copy registered.
    pub fn restore(&mut self, owner: &str, taken: Self) {
        self.remove_owner(owner);
        self.by_extension.extend(taken.by_extension);
    }

    /// The owner and importer for the file `path`.
    #[must_use]
    pub fn for_file(&self, path: &Path) -> Option<(&str, &Arc<dyn Importer>)> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.by_extension.get(&extension).map(|(o, i)| (o.as_str(), i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Upper;

    impl Importer for Upper {
        fn file_extensions(&self) -> &[&str] {
            &["dlg"]
        }

        fn import(&self, input: &Path, out: &mut ImportOutput) -> Result<(), ExtensionImportError> {
            let text = input.to_string_lossy().to_uppercase();
            out.files.push(("out.txt".into(), text.into_bytes()));
            Ok(())
        }
    }

    #[test]
    fn lookup_is_by_lowercase_extension_and_owner() {
        let mut registry = ImporterRegistry::default();
        registry.insert("DLG", "dialogue", Arc::new(Upper));
        let (owner, importer) = registry.for_file(Path::new("a/b.Dlg")).expect("found");
        assert_eq!(owner, "dialogue");
        let mut out = ImportOutput::default();
        importer.import(Path::new("x"), &mut out).expect("import");
        assert_eq!(out.files[0].1, b"X");
        assert!(registry.for_file(Path::new("a/b.png")).is_none());
        registry.remove_owner("dialogue");
        assert!(registry.for_file(Path::new("a/b.dlg")).is_none());
    }

    /// audit F13: the second owner of an extension does not displace the first.
    #[test]
    fn the_first_owner_of_an_extension_keeps_it() {
        let mut registry = ImporterRegistry::default();
        registry.insert("dlg", "a", Arc::new(Upper));
        registry.insert("dlg", "b", Arc::new(Upper));
        assert_eq!(registry.for_file(Path::new("x.dlg")).expect("held").0, "a");
        registry.remove_owner("b");
        assert_eq!(registry.for_file(Path::new("x.dlg")).expect("still held").0, "a");
        registry.insert("dlg", "a", Arc::new(Upper));
        assert!(registry.for_file(Path::new("x.dlg")).is_some());
    }
}
