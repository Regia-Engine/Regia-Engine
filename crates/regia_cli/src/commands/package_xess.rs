// SPDX-License-Identifier: MPL-2.0
//! Opt-in staging of the Intel XeSS Super Resolution runtime (ADR-4656, ADR-7820).
//! No SDK bytes or licence text are embedded in this module.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_render::upscaler::VendorUpscaler;
use regia_render::upscaler::vendor::{LibraryOs, RuntimeLibrary};

use super::Platform;

/// The SDK's licence (Intel Simplified Software License), staged beside the runtime.
pub(super) const LICENSE_NAME: &str = "INTEL-XESS-LICENSE.txt";
/// The SDK's `third-party-programs.txt`, staged beside the runtime.
pub(super) const PROGRAMS_NAME: &str = "INTEL-XESS-THIRD-PARTY-PROGRAMS.txt";

/// Validated release files. Creating this plan never copies anything.
pub(super) struct Plan {
    runtime: PathBuf,
    license: PathBuf,
    programs: PathBuf,
}

impl Plan {
    /// Explicit distribution opt-in (`REGIA_PACKAGE_XESS=1` with `XESS_SDK`), independent of
    /// whether the game links the XeSS binding (its `regia/xess` feature).
    pub(super) fn from_env(platform: Platform, server: bool) -> Result<Option<Self>> {
        let Some(enabled) = std::env::var_os("REGIA_PACKAGE_XESS") else {
            return Ok(None);
        };
        if enabled == "0" {
            return Ok(None);
        }
        if enabled != "1" || server || platform != Platform::Windows {
            bail!("xess_package_invalid: REGIA_PACKAGE_XESS=1 requires a Windows client package");
        }
        let sdk = std::env::var_os("XESS_SDK")
            .map(PathBuf::from)
            .context("xess_package_invalid: set XESS_SDK to the approved XeSS SDK root")?;
        Self::new(&sdk, platform).map(Some)
    }

    /// Validates the SDK bundle's runtime and terms for `platform`.
    pub(super) fn new(sdk: &Path, platform: Platform) -> Result<Self> {
        if !sdk.is_absolute() {
            bail!("xess_package_invalid: the SDK path must be absolute");
        }
        if platform != Platform::Windows {
            bail!("xess_package_invalid: XeSS ships a Windows runtime only");
        }
        let sdk = sdk
            .canonicalize()
            .context("xess_package_invalid: SDK directory")?;
        let bin = sdk.join("bin");
        let library = RuntimeLibrary::of(VendorUpscaler::Xess, LibraryOs::Windows);
        let runtime = library
            .locate(Some(&bin), None)
            .found
            .context("xess_package_invalid: the SDK has no bin/libxess.dll")?
            .canonicalize()
            .context("xess_package_invalid: runtime path")?;
        // A link to another library (the frame-generation or low-latency DLL) is refused.
        if runtime.parent() != Some(bin.as_path())
            || runtime.file_name().and_then(|name| name.to_str()) != Some("libxess.dll")
        {
            bail!("xess_package_invalid: the runtime must be the SDK's own bin/libxess.dll");
        }
        let license = sdk.join("LICENSE.txt");
        let programs = sdk.join("third-party-programs.txt");
        for path in [&runtime, &license, &programs] {
            let metadata = std::fs::metadata(path)
                .with_context(|| format!("xess_package_invalid: reading {}", path.display()))?;
            if !metadata.is_file() || metadata.len() == 0 {
                bail!("xess_package_invalid: {} must be a nonempty file", path.display());
            }
        }
        Ok(Self {
            runtime,
            license,
            programs,
        })
    }

    /// Copies the runtime beside the executable and both terms files, bytes unchanged (Intel's
    /// signature stays valid).
    pub(super) fn stage(&self, staging: &Path) -> Result<()> {
        for (source, destination) in [
            (&self.runtime, staging.join("libxess.dll")),
            (&self.license, staging.join(LICENSE_NAME)),
            (&self.programs, staging.join(PROGRAMS_NAME)),
        ] {
            std::fs::copy(source, &destination)
                .with_context(|| format!("xess_package_invalid: staging {}", destination.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::{unique_temp_dir, write_atomic};

    /// The SDK 3.0.2 bundle's layout: `bin/` holds the SR runtime beside the other features'.
    fn fixture() -> PathBuf {
        let root = unique_temp_dir("xess-package");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("SDK fixture directory");
        write_atomic(&bin.join("libxess.dll"), b"sr runtime").expect("runtime fixture");
        for other in ["libxess_fg.dll", "libxell.dll", "libxess_dx11.dll"] {
            write_atomic(&bin.join(other), b"do not stage").expect("other runtime");
        }
        write_atomic(&root.join("LICENSE.txt"), b"fixture SDK terms").expect("terms fixture");
        write_atomic(&root.join("third-party-programs.txt"), b"fixture programs").expect("programs fixture");
        root
    }

    #[test]
    fn xess_package_stages_only_the_sr_runtime_and_both_terms_files() {
        let root = fixture();
        let staging = unique_temp_dir("xess-package-staging");
        let plan = Plan::new(&root, Platform::Windows).expect("release plan");
        plan.stage(&staging).expect("stage");
        assert_eq!(
            std::fs::read(staging.join("libxess.dll")).expect("runtime"),
            b"sr runtime"
        );
        assert_eq!(
            std::fs::read(staging.join(LICENSE_NAME)).expect("terms"),
            b"fixture SDK terms"
        );
        assert_eq!(
            std::fs::read(staging.join(PROGRAMS_NAME)).expect("programs"),
            b"fixture programs"
        );
        for other in ["libxess_fg.dll", "libxell.dll", "libxess_dx11.dll"] {
            assert!(!staging.join(other).exists(), "{other} was staged");
        }
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
        std::fs::remove_dir_all(staging).expect("remove staging fixture");
    }

    #[test]
    fn xess_package_refuses_other_targets_relative_paths_and_missing_or_empty_files() {
        let root = fixture();
        for platform in [Platform::Linux, Platform::Android] {
            let error = Plan::new(&root, platform).err().expect("Windows only");
            assert!(error.to_string().starts_with("xess_package_invalid"), "{error}");
        }
        assert!(Plan::new(Path::new("relative/xess"), Platform::Windows).is_err());
        write_atomic(&root.join("third-party-programs.txt"), b"").expect("empty programs");
        assert!(Plan::new(&root, Platform::Windows).is_err());
        write_atomic(&root.join("third-party-programs.txt"), b"programs").expect("restore");
        std::fs::remove_file(root.join("LICENSE.txt")).expect("remove terms");
        assert!(Plan::new(&root, Platform::Windows).is_err());
        write_atomic(&root.join("LICENSE.txt"), b"terms").expect("restore terms");
        std::fs::remove_file(root.join("bin/libxess.dll")).expect("remove runtime");
        let error = Plan::new(&root, Platform::Windows).err().expect("no runtime");
        assert!(error.to_string().contains("bin/libxess.dll"), "{error}");
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
    }

    #[cfg(unix)]
    #[test]
    fn xess_package_refuses_a_runtime_link_to_another_library() {
        let root = fixture();
        let runtime = root.join("bin/libxess.dll");
        std::fs::remove_file(&runtime).expect("remove runtime");
        std::os::unix::fs::symlink("libxess_fg.dll", &runtime).expect("frame-generation redirect");
        let error = Plan::new(&root, Platform::Windows)
            .err()
            .expect("refuse another feature");
        assert!(error.to_string().contains("own bin/libxess.dll"), "{error}");
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
    }
}
