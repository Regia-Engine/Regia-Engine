// SPDX-License-Identifier: MPL-2.0
//! Download Khronos glTF sample models into a gitignored cache (`cargo xtask fetch-test-assets`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Khronos sample models required by specs/12 § 9 M4, with the licence each model's own README
/// states (checked 2026-10-30; the models are fetched for local tests and never redistributed).
/// DamagedHelmet is non-commercial and Sponza is under the Crytek agreement, so neither may be
/// in a published benchmark (`docs/benchmark-scenes.md`).
const MODELS: &[(&str, &str, &str)] = &[
    (
        "DamagedHelmet",
        "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Models/master/2.0/DamagedHelmet/glTF-Binary/DamagedHelmet.glb",
        "CC-BY-NC (non-commercial)",
    ),
    (
        "Sponza",
        "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Models/master/2.0/Sponza/glTF/Sponza.gltf",
        "Crytek CryEngine Limited License Agreement (restricted)",
    ),
    (
        "FlightHelmet",
        "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Models/master/2.0/FlightHelmet/glTF/FlightHelmet.gltf",
        "CC0-1.0",
    ),
    (
        "Fox",
        "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Models/master/2.0/Fox/glTF-Binary/Fox.glb",
        "CC0-1.0 (mesh) and CC-BY-4.0 (rig, animation)",
    ),
    (
        "CesiumMan",
        "https://raw.githubusercontent.com/KhronosGroup/glTF-Sample-Models/master/2.0/CesiumMan/glTF-Binary/CesiumMan.glb",
        "CC-BY-4.0",
    ),
];

/// Cache directory relative to workspace root.
pub fn cache_dir(root: &Path) -> PathBuf {
    root.join(".regia").join("test-assets").join("khronos-gltf")
}

/// The sibling file a download goes to before it is complete.
fn part_path(dest: &Path) -> PathBuf {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// Downloads into `<dest>.part` and renames it on success, so an interrupted download never
/// looks cached on the next run.
fn download(url: &str, dest: &Path) -> Result<()> {
    let part = part_path(dest);
    let status = Command::new("curl")
        .args(["-fsSL", url, "-o"])
        .arg(&part)
        .stdin(Stdio::null())
        .status()
        .context("spawn curl")?;
    if !status.success() {
        let _ = fs::remove_file(&part);
        anyhow::bail!("curl failed for {url}");
    }
    fs::rename(&part, dest).with_context(|| format!("renaming {} to {}", part.display(), dest.display()))
}

/// Download models when missing; record licences in the cache `LICENSES.txt`.
pub fn run(root: &Path) -> Result<()> {
    let dest = cache_dir(root);
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let notice = dest.join("LICENSES.txt");
    for (name, url, licence) in MODELS {
        let ext = if url.ends_with(".gltf") { "gltf" } else { "glb" };
        let out = dest.join(format!("{name}.{ext}"));
        if out.is_file() {
            println!("skip {name} (already cached)");
            // A sidecar that failed last time is fetched now (present ones are skipped).
            if ext == "gltf" {
                fetch_gltf_sidecars(&out, &dest, url)?;
            }
            continue;
        }
        println!("fetch {name} …");
        download(url, &out)?;
        if ext == "gltf" {
            fetch_gltf_sidecars(&out, &dest, url)?;
        }
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&notice)?;
        writeln!(f, "{name}: Khronos glTF Sample Models ({licence}) — {url}")?;
    }
    Ok(())
}

fn fetch_gltf_sidecars(gltf_path: &Path, cache_dir: &Path, gltf_url: &str) -> Result<()> {
    let json = fs::read_to_string(gltf_path)?;
    let base_url = gltf_url.rsplit_once('/').map(|(b, _)| b).unwrap_or(gltf_url);
    for uri in extract_uri_refs(&json) {
        if uri.starts_with("data:") {
            continue;
        }
        let target = cache_dir.join(&uri);
        if target.is_file() {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let remote = format!("{base_url}/{uri}");
        println!("fetch sidecar {uri} …");
        download(&remote, &target)?;
    }
    Ok(())
}

fn extract_uri_refs(json: &str) -> Vec<String> {
    let mut uris = Vec::new();
    for line in json.lines() {
        if let Some(quoted) = line
            .split("\"uri\"")
            .nth(1)
            .and_then(|rest| rest.split('"').nth(1))
        {
            uris.push(quoted.to_string());
        }
    }
    uris
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The licence labels stay the ones the models' READMEs state: a restricted model must not
    /// read as CC0 (it would end up in a published benchmark).
    #[test]
    fn restricted_models_are_not_labelled_cc0() {
        let licence = |name: &str| {
            MODELS
                .iter()
                .find(|m| m.0 == name)
                .map(|m| m.2)
                .expect("model listed")
        };
        assert!(!licence("DamagedHelmet").contains("CC0"));
        assert!(!licence("Sponza").contains("CC0"));
        assert_eq!(licence("FlightHelmet"), "CC0-1.0");
        assert!(licence("Fox").contains("CC-BY"));
        assert!(licence("CesiumMan").contains("CC-BY"));
    }

    #[test]
    fn a_download_goes_to_a_part_file_beside_the_destination() {
        let dest = Path::new("/cache/khronos-gltf/textures/Sponza_bump.png");
        assert_eq!(
            part_path(dest),
            Path::new("/cache/khronos-gltf/textures/Sponza_bump.png.part")
        );
        // A name without an extension keeps its whole name.
        assert_eq!(part_path(Path::new("/c/LICENSE")), Path::new("/c/LICENSE.part"));
    }
}
