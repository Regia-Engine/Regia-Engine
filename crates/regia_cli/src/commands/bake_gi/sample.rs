// SPDX-License-Identifier: MPL-2.0
//! `regia bake-gi --sample <volume.fgiv> --at x,y,z [--at …] [--normal nx,ny,nz]`: reads a
//! baked volume and prints the irradiance the runtime would light each point with (ADR-365,
//! `book/agents/baked-gi-bake-check.md`).
//!
//! The lookup is the renderer's own CPU mirror of the shader
//! ([`regia_render::sample_static_gi`]) with the default lookup settings and every component at
//! its baked light (the frame's sun and fill equal to the bake's). The viewer is taken along the
//! normal. Nothing needs a GPU.

use std::path::Path;

use anyhow::{Result, anyhow};
use regia_asset::baked_gi::{BakedGiVolume, GI_COMPONENTS, GiComponent};
use regia_render::{BakedGiSettings, StaticGiSample, sample_static_gi};
use serde_json::{Map, Value, json};

use super::BakeGiArgs;

/// Parses `x,y,z`.
///
/// # Errors
///
/// Not three finite numbers.
pub fn parse_vec3(text: &str) -> Result<[f32; 3], String> {
    let v: Vec<f32> = text
        .split(',')
        .map(|s| s.trim().parse::<f32>().map_err(|e| format!("`{s}`: {e}")))
        .collect::<Result<_, _>>()?;
    match v[..] {
        [x, y, z] if x.is_finite() && y.is_finite() && z.is_finite() => Ok([x, y, z]),
        _ => Err("expected three finite numbers: x,y,z".into()),
    }
}

/// A `--sample` error: a stable code (`volume_not_found`, `volume_unreadable`,
/// `sample_normal_invalid`) and the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleError {
    /// Error code (lowercase ascii, digits and `_`).
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for SampleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.message, self.code)
    }
}

impl std::error::Error for SampleError {}

/// Reads the volume at `path`: a split volume (ADR-2618) as its whole set from part 0, joined
/// back into the volume the bake split, so a point in any part samples the baked light.
///
/// # Errors
///
/// `volume_not_found` (no file) or `volume_unreadable` (unreadable, damaged, or another format
/// version), as `level.get_baked_gi` reports them.
pub fn read_volume(path: &Path) -> Result<BakedGiVolume, SampleError> {
    if !path.is_file() {
        return Err(SampleError {
            code: "volume_not_found",
            message: format!("{}: file not found", path.display()),
        });
    }
    BakedGiVolume::read_split_set(path)
        .and_then(|parts| BakedGiVolume::join_split_set(&parts))
        .map_err(|e| SampleError {
            code: "volume_unreadable",
            message: format!("{}: {e}", path.display()),
        })
}

/// The lookup at `at` with unit normal `normal` (viewer along the normal), at the baked light.
#[must_use]
pub fn sample_at(volume: &BakedGiVolume, at: [f32; 3], normal: [f32; 3]) -> StaticGiSample {
    let scales = [[1.0f32; 3]; GI_COMPONENTS];
    sample_static_gi(volume, &BakedGiSettings::default(), &scales, at, normal, normal)
}

fn lux(over_pi: [f32; 3]) -> [f32; 3] {
    over_pi.map(|v| v * std::f32::consts::PI)
}

/// One point's JSON: `at`, `normal`, `irradiance` (lux, per channel), `components` (lux per
/// component), `coverage`.
#[must_use]
pub fn sample_json(at: [f32; 3], normal: [f32; 3], s: &StaticGiSample) -> Value {
    let mut components = Map::new();
    for (c, value) in GiComponent::ALL.iter().zip(s.components) {
        components.insert(c.name().to_string(), json!(lux(value)));
    }
    json!({
        "at": at,
        "normal": normal,
        "irradiance": lux(s.irradiance_over_pi),
        "components": components,
        "coverage": s.coverage,
    })
}

/// The `--sample` summary for `volume` at `points`.
///
/// # Errors
///
/// `sample_normal_invalid` for a zero normal.
pub fn summary(
    volume: &BakedGiVolume,
    file: &Path,
    points: &[[f32; 3]],
    normal: [f32; 3],
) -> Result<Value, SampleError> {
    let len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    if !len.is_finite() || len <= 1e-6 {
        return Err(SampleError {
            code: "sample_normal_invalid",
            message: "the normal must be a non-zero vector".into(),
        });
    }
    let n = normal.map(|v| v / len);
    let valid = (0..volume.probe_count())
        .filter(|&i| volume.probe_meta(i).is_valid())
        .count();
    let lights = volume.lights;
    Ok(json!({
        "ok": true,
        "volume": file.display().to_string(),
        "probes": volume.probe_count(),
        "valid": valid,
        "bricks": volume.bricks.len(),
        "interior_bricks": volume.interior_brick_count(),
        "terrain": volume.terrain.is_some(),
        "baked": {
            "sun_illuminance": lights.sun_illuminance,
            "sun_direction": lights.sun_direction,
            "sky_scale": lights.sky_scale,
            "fill_lux": lights.fill_lux,
        },
        "samples": points.iter().map(|&p| sample_json(p, n, &sample_at(volume, p, n))).collect::<Vec<_>>(),
    }))
}

/// Runs `regia bake-gi --sample`: prints the summary, or `{"ok": false, "code", "message"}`
/// and fails.
///
/// # Errors
///
/// A [`SampleError`].
pub fn run(args: &BakeGiArgs) -> Result<()> {
    let Some(file) = args.sample.as_deref() else {
        return Err(anyhow!("--sample needs a volume"));
    };
    let result =
        read_volume(file).and_then(|v| summary(&v, file, &args.at, args.normal.unwrap_or([0.0, 1.0, 0.0])));
    match result {
        Ok(line) => {
            println!("{line}");
            Ok(())
        }
        Err(e) => {
            println!("{}", json!({ "ok": false, "code": e.code, "message": e.message }));
            Err(anyhow!(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::baked_gi::{BakeInfo, BakeLights, ProbeData, ProbeFlags, ProbeMeta, VolumeLayout};
    use regia_pt::probe::detail_brick;

    /// A 3 × 3 × 3 brick whose probes all carry sky radiance `l` (L00 only).
    fn uniform(l: f32) -> BakedGiVolume {
        uniform_bricks(l, &[0.0])
    }

    /// 3 × 3 × 3 bricks at x = each of `xs`, every probe carrying sky radiance `l` (L00 only).
    fn uniform_bricks(l: f32, xs: &[f32]) -> BakedGiVolume {
        let mut layout = VolumeLayout {
            terrain: None,
            bricks: xs
                .iter()
                .map(|&x| detail_brick([x, 0.0, 0.0], [x + 2.0, 2.0, 2.0], 1.0))
                .collect(),
            moment_resolution: 4,
            max_distance: 8.0,
        };
        layout.assign_brick_offsets();
        // Radiance SH of a uniform field: L00 = l·√(4π).
        let l00 = l * (4.0 * std::f32::consts::PI).sqrt();
        let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
        sh[1] = [[l00, 0.0, 0.0, 0.0]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh,
                moments: vec![[8.0, 64.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume")
    }

    /// Inside the brick a uniform sky of radiance L reads π·L lux on every normal, all of it in
    /// the sky component; outside the volume nothing (coverage 0).
    #[test]
    fn a_uniform_volume_reads_pi_l_inside_and_nothing_outside() {
        let v = uniform(2.0);
        let line = summary(
            &v,
            Path::new("u.fgiv"),
            &[[1.0; 3], [50.0, 0.0, 0.0]],
            [0.0, 0.0, 3.0],
        )
        .expect("summary");
        assert_eq!(line["ok"], true);
        assert_eq!(line["probes"], 27);
        assert_eq!(line["valid"], 27);
        let inside = &line["samples"][0];
        assert_eq!(inside["normal"], json!([0.0, 0.0, 1.0]), "normalized");
        let pi_l = std::f64::consts::PI * 2.0;
        for (got, comp) in [
            (&inside["irradiance"][0], "total"),
            (&inside["components"]["sky"][1], "sky"),
        ] {
            let got = got.as_f64().expect("number");
            assert!((got - pi_l).abs() < 0.03 * pi_l, "{comp}: {got} vs {pi_l}");
        }
        assert_eq!(inside["components"]["sun_bounce"], json!([0.0, 0.0, 0.0]));
        assert_eq!(inside["coverage"], 1.0);
        assert_eq!(line["samples"][1]["coverage"], 0.0);
        assert_eq!(line["samples"][1]["irradiance"], json!([0.0, 0.0, 0.0]));
    }

    /// ADR-2618: a split volume samples as the whole bake: read from part 0, a point in part 1
    /// reads the baked light (part 0 alone holds nothing there), and a missing part is
    /// `volume_unreadable`.
    #[test]
    fn a_split_volume_samples_every_part() {
        let whole = uniform_bricks(2.0, &[0.0, 40.0]);
        let cap = whole.layout().runtime_buffer_bytes() * 3 / 4;
        let parts = whole.split_parts(cap).expect("two bricks split");
        assert_eq!(parts.len(), 2);
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_sample_split");
        let file = dir.join("two.fgiv");
        let written = regia_asset::baked_gi_split::write_split_set(&parts, &file).expect("write parts");
        let read = read_volume(&file).expect("the set reads");
        assert!(read == whole, "the joined set is the bake");
        let line = summary(&read, &file, &[[1.0; 3], [41.0, 1.0, 1.0]], [0.0, 1.0, 0.0]).expect("summary");
        assert_eq!(line["probes"], 54);
        let pi_l = std::f64::consts::PI * 2.0;
        for k in 0..2 {
            let got = line["samples"][k]["irradiance"][0].as_f64().expect("number");
            assert!((got - pi_l).abs() < 0.03 * pi_l, "sample {k}: {got} vs {pi_l}");
        }
        let far = sample_at(&parts[0], [41.0, 1.0, 1.0], [0.0, 1.0, 0.0]);
        assert_eq!(far.coverage, 0.0, "part 0 alone does not reach the second brick");
        std::fs::remove_file(&written[1]).expect("remove part 1");
        assert_eq!(
            read_volume(&file).expect_err("part 1 missing").code,
            "volume_unreadable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_inputs_have_codes() {
        assert!(parse_vec3("1,2").is_err());
        assert!(parse_vec3("1,2,nan").is_err());
        assert_eq!(parse_vec3(" -1, 2.5 ,3"), Ok([-1.0, 2.5, 3.0]));
        let dir = regia_test_utils::unique_temp_dir("regia_cli_bake_sample");
        assert_eq!(
            read_volume(&dir.join("none.fgiv")).expect_err("missing").code,
            "volume_not_found"
        );
        let bad = dir.join("bad.fgiv");
        regia_test_utils::write_atomic(&bad, b"not a volume").expect("write");
        assert_eq!(read_volume(&bad).expect_err("damaged").code, "volume_unreadable");
        let err = summary(&uniform(1.0), &bad, &[[0.0; 3]], [0.0; 3]).expect_err("zero normal");
        assert_eq!(err.code, "sample_normal_invalid");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
