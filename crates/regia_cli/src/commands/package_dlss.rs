// SPDX-License-Identifier: MPL-2.0
//! Opt-in staging of the approved DLSS SR release runtime (ADR-4758).
//! No SDK bytes or license text are embedded in this module.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_render::upscaler::VendorUpscaler;
use regia_render::upscaler::vendor::{LibraryOs, RuntimeLibrary};

use super::Platform;

const LICENSE_NAME: &str = "NVIDIA-DLSS-LICENSE.txt";
const NOTICES_NAME: &str = "NVIDIA-DLSS-THIRD-PARTY-NOTICES.txt";

/// Validated release files. Creating this plan never copies anything.
pub(super) struct Plan {
    runtime: PathBuf,
    license: PathBuf,
    notices: PathBuf,
}

impl Plan {
    /// Explicit distribution opt-in, independent of whether the CLI itself links NGX.
    pub(super) fn from_env(platform: Platform, server: bool) -> Result<Option<Self>> {
        let Some(enabled) = std::env::var_os("REGIA_PACKAGE_DLSS") else {
            return Ok(None);
        };
        if enabled == "0" {
            return Ok(None);
        }
        if enabled != "1" || server || platform == Platform::Android {
            bail!("dlss_package_invalid: REGIA_PACKAGE_DLSS=1 requires a desktop client package");
        }
        let sdk = std::env::var_os("DLSS_SDK")
            .map(PathBuf::from)
            .context("dlss_package_invalid: set DLSS_SDK to the approved SDK root")?;
        let notices = std::env::var_os("REGIA_DLSS_NOTICES")
            .map(PathBuf::from)
            .context(
                "dlss_package_invalid: set REGIA_DLSS_NOTICES to the complete runtime third-party notices",
            )?;
        Self::new(&sdk, &notices, platform).map(Some)
    }

    fn new(sdk: &Path, notices: &Path, platform: Platform) -> Result<Self> {
        if !sdk.is_absolute() || !notices.is_absolute() {
            bail!("dlss_package_invalid: SDK and notice paths must be absolute");
        }
        let (directory, os) = match platform {
            Platform::Linux if std::env::consts::ARCH == "x86_64" => ("Linux_x86_64", LibraryOs::Linux),
            Platform::Windows => ("Windows_x86_64", LibraryOs::Windows),
            _ => bail!("dlss_package_invalid: no DLSS runtime for this package target"),
        };
        let sdk = sdk
            .canonicalize()
            .context("dlss_package_invalid: SDK directory")?;
        let release = sdk.join("lib").join(directory).join("rel");
        // A rel-directory symlink to dev defeats the release-only selection.
        if release
            .canonicalize()
            .context("dlss_package_invalid: release directory")?
            != release
        {
            bail!("dlss_package_invalid: release directory must not redirect outside its SDK location");
        }
        let library = RuntimeLibrary::of(VendorUpscaler::Dlss, os);
        let runtime = library
            .locate(Some(&release), None)
            .found
            .context("dlss_package_invalid: SR release runtime missing (dev is never a fallback)")?
            .canonicalize()
            .context("dlss_package_invalid: runtime path")?;
        if runtime.parent() != Some(release.as_path()) {
            bail!("dlss_package_invalid: release runtime must remain inside the rel directory");
        }
        if !runtime
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                library
                    .names
                    .iter()
                    .any(|candidate| candidate.version_of(name).is_some())
            })
        {
            bail!("dlss_package_invalid: resolved runtime must be the SR library");
        }
        let license = sdk.join("LICENSE.txt");
        for path in [&runtime, &license, notices] {
            let metadata = std::fs::metadata(path)
                .with_context(|| format!("dlss_package_invalid: reading {}", path.display()))?;
            if !metadata.is_file() || metadata.len() == 0 {
                bail!("dlss_package_invalid: {} must be a nonempty file", path.display());
            }
        }
        Ok(Self {
            runtime,
            license,
            notices: notices.to_path_buf(),
        })
    }

    /// Copy bytes without stripping, patching or re-signing NVIDIA's runtime.
    pub(super) fn stage(&self, staging: &Path) -> Result<()> {
        let runtime_name = self
            .runtime
            .file_name()
            .context("dlss_package_invalid: runtime has no file name")?;
        for (source, destination) in [
            (&self.runtime, staging.join(runtime_name)),
            (&self.license, staging.join(LICENSE_NAME)),
            (&self.notices, staging.join(NOTICES_NAME)),
        ] {
            std::fs::copy(source, &destination)
                .with_context(|| format!("dlss_package_invalid: staging {}", destination.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::{unique_temp_dir, write_atomic};

    fn fixture() -> (PathBuf, PathBuf) {
        let root = unique_temp_dir("dlss-package");
        for platform in ["Windows_x86_64", "Linux_x86_64"] {
            for flavor in ["rel", "dev"] {
                let dir = root.join("lib").join(platform).join(flavor);
                std::fs::create_dir_all(&dir).expect("SDK fixture directory");
                let name = if platform.starts_with("Windows") {
                    "nvngx_dlss.dll"
                } else {
                    "libnvidia-ngx-dlss.so.310.9.16"
                };
                write_atomic(&dir.join(name), flavor.as_bytes()).expect("runtime fixture");
                write_atomic(&dir.join("nvngx_dlssg.dll"), b"do not stage").expect("other runtime");
            }
        }
        write_atomic(&root.join("LICENSE.txt"), b"fixture SDK terms").expect("terms fixture");
        let notices = root.join("complete-runtime-notices.txt");
        write_atomic(&notices, b"fixture third-party notices").expect("notice fixture");
        (root, notices)
    }

    #[test]
    fn dlss_package_stages_only_release_sr_and_both_notices() {
        let (root, notices) = fixture();
        let staging = unique_temp_dir("dlss-package-staging");
        for platform in [Platform::Windows, Platform::Linux] {
            if platform == Platform::Linux && std::env::consts::ARCH != "x86_64" {
                continue;
            }
            let plan = Plan::new(&root, &notices, platform).expect("release plan");
            plan.stage(&staging).expect("stage release");
            let runtime_name = plan.runtime.file_name().expect("runtime filename");
            assert_eq!(
                std::fs::read(staging.join(runtime_name)).expect("runtime"),
                b"rel"
            );
            assert_eq!(
                std::fs::read(staging.join(LICENSE_NAME)).expect("terms"),
                b"fixture SDK terms"
            );
            assert_eq!(
                std::fs::read(staging.join(NOTICES_NAME)).expect("notices"),
                b"fixture third-party notices"
            );
            assert!(!staging.join("nvngx_dlssg.dll").exists());
            std::fs::remove_file(staging.join(runtime_name)).expect("remove runtime fixture");
        }
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
        std::fs::remove_dir_all(staging).expect("remove staging fixture");
    }

    #[test]
    fn dlss_package_refuses_missing_release_even_with_a_dev_runtime() {
        let (root, notices) = fixture();
        std::fs::remove_file(root.join("lib/Windows_x86_64/rel/nvngx_dlss.dll")).expect("remove rel");
        let error = Plan::new(&root, &notices, Platform::Windows)
            .err()
            .expect("refuse dev");
        assert!(error.to_string().contains("dev is never a fallback"));
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
    }

    #[test]
    fn dlss_package_requires_nonempty_terms_and_notices_and_absolute_paths() {
        let (root, notices) = fixture();
        assert!(Plan::new(Path::new("relative"), &notices, Platform::Windows).is_err());
        assert!(Plan::new(&root, Path::new("relative"), Platform::Windows).is_err());
        assert!(Plan::new(&root, &notices, Platform::Android).is_err());
        write_atomic(&notices, b"").expect("empty notices");
        assert!(Plan::new(&root, &notices, Platform::Windows).is_err());
        write_atomic(&notices, b"notices").expect("restore notices");
        std::fs::remove_file(root.join("LICENSE.txt")).expect("remove terms");
        assert!(Plan::new(&root, &notices, Platform::Windows).is_err());
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
    }

    #[cfg(unix)]
    #[test]
    fn dlss_package_refuses_a_release_symlink_to_the_dev_runtime() {
        let (root, notices) = fixture();
        let runtime = root.join("lib/Windows_x86_64/rel/nvngx_dlss.dll");
        std::fs::remove_file(&runtime).expect("remove rel");
        std::os::unix::fs::symlink("../dev/nvngx_dlss.dll", &runtime).expect("dev redirect");
        let error = Plan::new(&root, &notices, Platform::Windows)
            .err()
            .expect("refuse dev redirect");
        assert!(error.to_string().contains("inside the rel directory"));
        std::fs::remove_file(&runtime).expect("remove dev redirect");
        std::os::unix::fs::symlink("nvngx_dlssg.dll", &runtime).expect("other feature redirect");
        let error = Plan::new(&root, &notices, Platform::Windows)
            .err()
            .expect("refuse another feature");
        assert!(error.to_string().contains("must be the SR library"));
        std::fs::remove_dir_all(root).expect("remove SDK fixture");
    }
}
