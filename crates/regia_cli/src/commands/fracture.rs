// SPDX-License-Identifier: MPL-2.0
//! `regia fracture` (ADR-1597): pre-fracture a closed mesh and cook the runtime chunk asset.
//!
//! Input is a closed `.glb` mesh (its first mesh), an existing `.fracture.ron` (cook only), or
//! `--box X,Y,Z` (a box of those half extents). Output is `<out>.fracture.ron` (the chunk meshes and
//! joints, `regia_asset_pipeline::fracture`, ADR-1567; not written for a `.fracture.ron` input) and
//! `<out>.destructible.ron` (`regia_physics::destruction`, the asset the game loads). Both are
//! written atomically (temporary file, then rename). `--json` prints a summary for tools.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia::physics::destruction::{DestructionMaterial, SourceKit};
use regia_asset_pipeline::destructible_cook::{CookSettings, SupportRule, cook_destructible};
use regia_asset_pipeline::fracture::{
    FractureSet, FractureSettings, FractureSource, box_mesh, fracture_source,
};

/// `regia fracture` arguments.
#[derive(Debug, Args)]
pub struct FractureArgs {
    /// A closed `.glb` mesh, or a `.fracture.ron` to cook without fracturing again.
    pub input: Option<PathBuf>,
    /// Fracture a box with these half extents in metres instead of reading a file (`X,Y,Z`).
    #[arg(long = "box", value_name = "X,Y,Z")]
    pub box_half: Option<String>,
    /// Output path without the suffixes: writes `<out>.fracture.ron` and `<out>.destructible.ron`.
    #[arg(long)]
    pub out: PathBuf,
    /// Number of chunks to ask for.
    #[arg(long, default_value_t = 24)]
    pub chunks: usize,
    /// Seed of the site sampling (the same seed gives the same chunks).
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Newtons of joint break force per square metre of contact.
    #[arg(long, default_value_t = 500_000.0)]
    pub strength_per_area: f32,
    /// Kilograms per cubic metre.
    #[arg(long, default_value_t = 2400.0)]
    pub density: f32,
    /// Seconds a joint's break force must act to break its bond (break impulse = force x time).
    #[arg(long, default_value_t = 0.01)]
    pub bond_seconds: f32,
    /// Chunks whose lowest point is at or below this height are held by the world. Default: the
    /// lowest centimetre of the mesh.
    #[arg(long, value_name = "Y")]
    pub support_below_y: Option<f32>,
    /// No support chunks (a free object).
    #[arg(long, default_value_t = false, conflicts_with = "support_below_y")]
    pub free: bool,
    /// UV units per metre on the cut faces.
    #[arg(long, default_value_t = 1.0)]
    pub interior_uv_scale: f32,
    /// Sound/VFX tag of the material.
    #[arg(long, default_value = "default")]
    pub fx_tag: String,
    /// Kit id the set fractures (names the chunks `frac_<kit>_<n>`; needs `--kit-revision`).
    #[arg(long, requires = "kit_revision")]
    pub kit: Option<String>,
    /// Revision of the kit the set was cut from.
    #[arg(long, requires = "kit")]
    pub kit_revision: Option<String>,
    /// Print a JSON summary on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// What `regia fracture` wrote.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FractureSummary {
    /// The `.fracture.ron` written (absent when the input was one).
    pub fracture: Option<PathBuf>,
    /// The `.destructible.ron` written.
    pub destructible: PathBuf,
    /// Chunks.
    pub chunks: usize,
    /// Bonds.
    pub bonds: usize,
    /// Support chunks.
    pub supports: usize,
    /// Total mass, kilograms.
    pub mass_kg: f64,
}

/// Runs `regia fracture`.
///
/// # Errors
/// Bad arguments, an unreadable or open mesh, a fracture or cook error (the message carries the
/// error code), or a file that cannot be written.
pub fn run(args: &FractureArgs) -> Result<()> {
    let summary = fracture(args)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).context("summary json")?
        );
    } else {
        if let Some(f) = &summary.fracture {
            println!("fracture set: {}", f.display());
        }
        println!(
            "destructible: {} ({} chunks, {} bonds, {} supports, {:.1} kg)",
            summary.destructible.display(),
            summary.chunks,
            summary.bonds,
            summary.supports,
            summary.mass_kg
        );
    }
    Ok(())
}

/// Does the work of [`run`] and returns the summary.
///
/// # Errors
/// See [`run`].
pub fn fracture(args: &FractureArgs) -> Result<FractureSummary> {
    let name = args
        .out
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .context("--out needs a file name")?
        .to_string();
    let (set, fracture_written) = match (&args.input, &args.box_half) {
        (Some(_), Some(_)) => bail!("give an input file or --box, not both"),
        (None, None) => bail!("give an input file (.glb or .fracture.ron) or --box X,Y,Z"),
        (Some(path), None) if path.to_string_lossy().ends_with(".fracture.ron") => {
            let text =
                std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let set = FractureSet::from_ron(&text).map_err(|e| anyhow::anyhow!("{} ({})", e, e.code()))?;
            (set, false)
        }
        (Some(path), None) => {
            let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let mesh = regia_asset_pipeline::import_gltf(&bytes, regia_asset_pipeline::Forward::Convert)
                .with_context(|| format!("importing {}", path.display()))?;
            let source = FractureSource {
                positions: &mesh.positions,
                indices: &mesh.indices,
                normals: (mesh.normals.len() == mesh.positions.len()).then_some(mesh.normals.as_slice()),
                uvs: (mesh.uvs.len() == mesh.positions.len()).then_some(mesh.uvs.as_slice()),
            };
            (run_fracture(&source, args)?, true)
        }
        (None, Some(spec)) => {
            let half = parse_half(spec)?;
            let (positions, indices) = box_mesh(half).map_err(|e| anyhow::anyhow!("{} ({})", e, e.code()))?;
            let source = FractureSource {
                positions: &positions,
                indices: &indices,
                normals: None,
                uvs: None,
            };
            (run_fracture(&source, args)?, true)
        }
    };
    let fracture_path = with_suffix(&args.out, ".fracture.ron");
    let mut cook = CookSettings::new(&name);
    cook.meshes = Some(
        fracture_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    cook.density = args.density;
    cook.bond_seconds = args.bond_seconds;
    cook.support = if args.free {
        SupportRule::None
    } else {
        SupportRule::BelowY(args.support_below_y.unwrap_or(f32::NEG_INFINITY))
    };
    cook.material = DestructionMaterial {
        fx_tag: args.fx_tag.clone(),
        ..DestructionMaterial::default()
    };
    cook.source = args
        .kit
        .clone()
        .zip(args.kit_revision.clone())
        .map(|(kit, revision)| SourceKit { kit, revision });
    // The seed made the set only when this run fractured it.
    cook.fracture_seed = fracture_written.then_some(args.seed);
    if !fracture_written {
        // The input set is the mesh source: point at it.
        cook.meshes = args
            .input
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
    }
    let asset = cook_destructible(&set, &cook).map_err(|e| anyhow::anyhow!("{} ({})", e, e.code()))?;
    if fracture_written {
        let text = set
            .to_ron()
            .map_err(|e| anyhow::anyhow!("{} ({})", e, e.code()))?;
        write_atomic(&fracture_path, &text)?;
    }
    let destructible_path = with_suffix(&args.out, ".destructible.ron");
    let text = asset
        .to_ron()
        .map_err(|e| anyhow::anyhow!("{} ({})", e, e.code()))?;
    write_atomic(&destructible_path, &text)?;
    Ok(FractureSummary {
        fracture: fracture_written.then_some(fracture_path),
        destructible: destructible_path,
        chunks: asset.chunks.len(),
        bonds: asset.bonds.len(),
        supports: asset.support_chunks().len(),
        mass_kg: asset.total_mass(),
    })
}

fn run_fracture(source: &FractureSource<'_>, args: &FractureArgs) -> Result<FractureSet> {
    let mut settings = FractureSettings::random(args.chunks, args.seed);
    settings.joints.strength_per_area = args.strength_per_area;
    settings.render.interior_uv_scale = args.interior_uv_scale;
    fracture_source(source, &settings).map_err(|e| anyhow::anyhow!("{} ({})", e, e.code()))
}

fn parse_half(spec: &str) -> Result<[f32; 3]> {
    let parts: Vec<f32> = spec
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("--box `{spec}` must be three numbers X,Y,Z"))?;
    match parts.as_slice() {
        [x, y, z] => Ok([*x, *y, *z]),
        _ => bail!("--box `{spec}` must be three numbers X,Y,Z"),
    }
}

fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut s = stem.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Writes `text` to a temporary file next to `path`, then renames it over `path`.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = with_suffix(path, &format!(".tmp{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(out: PathBuf) -> FractureArgs {
        FractureArgs {
            input: None,
            box_half: Some("1,0.5,0.25".into()),
            out,
            chunks: 24,
            seed: 3,
            strength_per_area: 500_000.0,
            density: 2400.0,
            bond_seconds: 0.01,
            support_below_y: None,
            free: false,
            interior_uv_scale: 1.0,
            fx_tag: "concrete".into(),
            kit: Some("slab".into()),
            kit_revision: Some("r3".into()),
            json: false,
        }
    }

    #[test]
    fn box_writes_both_files_and_cooks_again_from_the_set() {
        let dir = regia_test_utils::unique_temp_dir("fracture-cli");
        let a = args(dir.join("slab"));
        let s = fracture(&a).expect("fracture");
        let fracture_path = s.fracture.clone().expect("fracture written");
        assert!(fracture_path.ends_with("slab.fracture.ron"));
        assert!(s.destructible.ends_with("slab.destructible.ron"));
        assert_eq!(s.chunks, 24);
        assert!(s.supports > 0 && s.supports < 24, "{s:?}");
        let text = std::fs::read_to_string(&s.destructible).expect("read");
        let asset = regia::physics::destruction::DestructibleAsset::from_ron(&text).expect("parse");
        assert_eq!(asset.meshes.as_deref(), Some("slab.fracture.ron"));
        assert_eq!(asset.material.fx_tag, "concrete");
        assert_eq!(asset.fracture_seed, Some(3));
        assert_eq!(asset.source.as_ref().map(|s| s.revision.as_str()), Some("r3"));
        assert_eq!(asset.chunks[7].name, "frac_slab_7");
        // Cooking from the written set gives the same asset (with the set as its mesh source).
        let mut again = args(dir.join("slab2"));
        again.box_half = None;
        again.input = Some(fracture_path);
        let s2 = fracture(&again).expect("cook");
        assert!(s2.fracture.is_none());
        let text2 = std::fs::read_to_string(&s2.destructible).expect("read");
        let asset2 = regia::physics::destruction::DestructibleAsset::from_ron(&text2).expect("parse");
        assert_eq!(asset2.chunks, asset.chunks);
        assert_eq!(asset2.bonds, asset.bonds);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_arguments_are_errors() {
        let dir = regia_test_utils::unique_temp_dir("fracture-cli-bad");
        let mut a = args(dir.join("x"));
        a.box_half = Some("1,2".into());
        assert!(fracture(&a).is_err());
        let mut a = args(dir.join("x"));
        a.box_half = None;
        assert!(fracture(&a).is_err(), "no input");
        let mut a = args(dir.join("x"));
        a.input = Some(dir.join("missing.glb"));
        assert!(fracture(&a).is_err(), "both inputs");
        let mut a = args(dir.join("x"));
        a.density = 0.0;
        let e = fracture(&a).expect_err("density");
        assert!(e.to_string().contains("destruction_invalid_asset"), "{e}");
        let mut a = args(dir.join("x"));
        a.chunks = 0;
        let e = fracture(&a).expect_err("chunks");
        assert!(e.to_string().contains("fracture_invalid_settings"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
