// SPDX-License-Identifier: MPL-2.0
//! Burley subsurface scattering in the tracer (ADR-2725) against the analytic profile.
//!
//! A large flat plate faces the camera; a thin black wall standing on the plate along `x = 0`
//! shadows one half from a sun at 45°, so the light that lands is a hard half-plane. The camera
//! sees the wall edge-on (no pixel centre lies on it). With the whole diffuse share scattering
//! (`tint = 1`) and no specular (IOR 1.001), every pixel's direct light is
//!
//! ```text
//! L(s) = ρ/π · E cos 45° · P(s),   P(s) = ∫ 2π r R(r) · (lit share of the circle of radius r) dr
//! ```
//!
//! for a point `s` metres into the lit side, `R` Burley's normalized diffusion (Christensen &
//! Burley 2015) with each channel's `d`. The test integrates `P` on the CPU and holds the traced
//! columns (averaged over rows) to it, and the far lit side to `ρ/π · E cos 45°` (the profile
//! integrates to one: the plate neither gains nor loses light).
//!
//! The plate is a material graph (a constant colour from a parameter), so the graph path and the
//! subsurface branch run in one tracer build: the scattered light takes the graph's colour.

use std::collections::BTreeMap;
use std::f64::consts::PI;

use regia_core::glam::Mat4;
use regia_pt::{
    Aov, PathTracer, PtCamera, PtMaterial, PtMesh, PtScene, PtSettings, PtSubsurface, PtSun, PtSurfaceGraph,
};
use regia_render::RenderDevice;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 24;
/// Half the view's width on the plate, metres.
const HALF_WIDTH: f64 = 0.5;
const EYE_HEIGHT: f32 = 20.0;
const ILLUMINANCE: f32 = 1000.0;
/// Burley's `d` per channel, metres.
const D: [f32; 3] = [0.01, 0.02, 0.04];
/// The graph's base colour (linear).
const COLOR: [f32; 3] = [0.8, 0.6, 0.4];

const PLATE_GRAPH: &str = r#"(
    format_version: 1,
    name: "sss_plate",
    params: [
        (name: "tint", value: Color((0.8, 0.6, 0.4))),
    ],
    textures: [],
    nodes: {
        "tint": Param("tint"),
        "rough": Float(1.0),
    },
    output: (
        base_color: Some("tint"),
        roughness: Some("rough"),
    ),
)"#;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> PtMesh {
    PtMesh {
        positions: corners.to_vec(),
        normals: vec![normal; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        colors: Vec::new(),
        tangents: Vec::new(),
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// The plate scene; `graph` makes the plate a material graph, `tint` its scattered share.
fn scene(graph: bool, tint: f32) -> PtScene {
    let fov_y = 2.0 * (HALF_WIDTH as f32 * HEIGHT as f32 / WIDTH as f32 / EYE_HEIGHT).atan();
    let mut scene = PtScene::new(PtCamera::pinhole(
        [0.0, 0.0, EYE_HEIGHT],
        [0.0, 0.0, 0.0],
        fov_y,
        WIDTH,
        HEIGHT,
    ));
    let plate = scene.add_material(PtMaterial {
        base_color: if graph {
            // The graph replaces the colour: a factor the graph ignores proves it does.
            [0.1, 0.9, 0.1, 1.0]
        } else {
            [COLOR[0], COLOR[1], COLOR[2], 1.0]
        },
        roughness: 1.0,
        ior: 1.001,
        ..PtMaterial::default()
    });
    let wall = scene.add_material(PtMaterial::diffuse([0.0; 3]));
    let s = 3.0;
    let plate_mesh = scene.add_mesh(quad(
        [[-s, -s, 0.0], [s, -s, 0.0], [s, s, 0.0], [-s, s, 0.0]],
        [0.0, 0.0, 1.0],
    ));
    let wall_mesh = scene.add_mesh(quad(
        [[0.0, -s, 0.0], [0.0, s, 0.0], [0.0, s, 10.0], [0.0, -s, 10.0]],
        [1.0, 0.0, 0.0],
    ));
    scene.add_instance(plate_mesh, plate, Mat4::IDENTITY);
    scene.add_instance(wall_mesh, wall, Mat4::IDENTITY);
    let h = std::f32::consts::FRAC_1_SQRT_2;
    scene.sun = Some(PtSun {
        toward: [-h, 0.0, h],
        color: [1.0; 3],
        illuminance: ILLUMINANCE,
        angular_radius: 0.0,
    });
    scene.subsurface.push(PtSubsurface {
        material: plate,
        d: D,
        tint: [tint; 3],
    });
    if graph {
        let parsed = regia_matgraph::MaterialGraph::from_ron(PLATE_GRAPH).expect("graph");
        let compiled = regia_matgraph::compile(&parsed).expect("compile");
        let lab = regia_matgraph::lab_material(
            &parsed,
            &compiled,
            regia_matgraph::default_params(&parsed, 0.0),
            &BTreeMap::new(),
        )
        .expect("material");
        let g = lab.surface_graph.as_deref().expect("graph material");
        scene.surface_graphs.push(PtSurfaceGraph {
            material: plate,
            surface: regia_render::surface_graph::surface_source(&g.code())
                .expect("surface")
                .into(),
            params: g.params,
            textures: [None; 7],
            vertex_offset: false,
            uses_uv1: false,
            uses_time: false,
        });
    }
    scene
}

/// The lit share of the half-plane response at `s` metres into the lit side, for Burley's `d`.
fn edge_response(d: f64, s: f64) -> f64 {
    let steps = 20_000;
    let r_max = 60.0 * d;
    let dr = r_max / f64::from(steps);
    let mut sum = 0.0;
    for k in 0..steps {
        let r = (f64::from(k) + 0.5) * dr;
        let density = ((-r / d).exp() + (-r / (3.0 * d)).exp()) / (4.0 * d);
        let a = s.abs();
        let lit = if s >= 0.0 {
            if r <= a { 1.0 } else { 1.0 - (a / r).acos() / PI }
        } else if r <= a {
            0.0
        } else {
            (a / r).acos() / PI
        };
        sum += density * lit * dr;
    }
    sum
}

/// Column means of the direct AOV, and each column's signed distance into the lit side.
fn columns(img: &regia_pt::PtImage) -> (Vec<[f64; 3]>, Vec<f64>) {
    let direct = img.rgb(Aov::Direct);
    let (w, h) = (img.width as usize, img.height as usize);
    let mut means = vec![[0.0f64; 3]; w];
    for y in 0..h {
        for (x, m) in means.iter_mut().enumerate() {
            let c = direct[y * w + x];
            for k in 0..3 {
                m[k] += f64::from(c[k]) / h as f64;
            }
        }
    }
    // The lit side is x < 0 (the sun is toward -x): find which screen side that is.
    let left: f64 = means[..w / 4].iter().map(|m| m[0]).sum();
    let right: f64 = means[3 * w / 4..].iter().map(|m| m[0]).sum();
    let lit_left = left > right;
    let s = (0..w)
        .map(|x| {
            let screen = ((x as f64 + 0.5) / w as f64 * 2.0 - 1.0) * HALF_WIDTH;
            if lit_left { -screen } else { screen }
        })
        .collect();
    (means, s)
}

#[test]
fn a_subsurface_plate_matches_burleys_half_plane_response_and_conserves_energy() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    // One sample picks the scattered half and one channel's radius (a one-sample mixture), so a
    // single sample of a channel carries a weight of about 6 a sixth of the time: per-sample
    // variance about 5 in units of the plain diffuse value. 256 spp × 24 rows × 8 columns per bin
    // leaves a standard error near 0.01 per bin (lavapipe 11-05: worst bin 0.011, mean 0.002, far-lit
    // energy within 0.7 %); the bounds below are about 3 standard errors.
    let settings = PtSettings {
        spp: 256,
        max_bounces: 1,
        jitter: false,
        seed: 7,
        ..PtSettings::default()
    };
    let img = tracer.render(&scene(true, 1.0), &settings).expect("trace");
    assert_eq!(
        img.metadata.surface_graphs.graphs, 1,
        "{:?}",
        img.metadata.surface_graphs
    );
    assert!(img.metadata.surface_graphs.refusals.is_empty());
    let (means, s) = columns(&img);
    let flat = [0, 1, 2]
        .map(|c| f64::from(COLOR[c]) / PI * f64::from(ILLUMINANCE) * std::f64::consts::FRAC_1_SQRT_2);
    // Bins of 8 columns (4 cm): the traced mean against the analytic mean over the same columns.
    // The two columns beside the wall's line are left out (no pixel centre lies on it).
    const BIN: usize = 8;
    let mut worst = [0.0f64; 3];
    let mut mean_err = [0.0f64; 3];
    let mut bins = 0usize;
    for first in (0..means.len()).step_by(BIN) {
        let cols: Vec<usize> = (first..(first + BIN).min(means.len()))
            .filter(|&x| s[x].abs() >= 0.006)
            .collect();
        if cols.is_empty() {
            continue;
        }
        bins += 1;
        for c in 0..3 {
            let traced = cols.iter().map(|&x| means[x][c] / flat[c]).sum::<f64>() / cols.len() as f64;
            let expected = cols
                .iter()
                .map(|&x| edge_response(f64::from(D[c]), s[x]))
                .sum::<f64>()
                / cols.len() as f64;
            let err = (traced - expected).abs();
            worst[c] = worst[c].max(err);
            mean_err[c] += err;
        }
    }
    let mean_err = mean_err.map(|e| e / bins as f64);
    // Far on the lit side (the 20 outermost lit columns) the profile has given back all it took:
    // the plain diffuse value.
    let far: Vec<usize> = {
        let mut lit: Vec<usize> = (0..means.len()).filter(|&x| s[x] > 0.0).collect();
        lit.sort_by(|a, b| s[*b].total_cmp(&s[*a]));
        lit.truncate(20);
        lit
    };
    let energy = [0, 1, 2].map(|c| {
        let traced: f64 = far.iter().map(|&x| means[x][c] / flat[c]).sum();
        let expected: f64 = far.iter().map(|&x| edge_response(f64::from(D[c]), s[x])).sum();
        traced / expected
    });
    eprintln!("burley slab: worst bin error {worst:?}, mean {mean_err:?}, far-lit energy {energy:?}");
    for c in 0..3 {
        assert!(
            (energy[c] - 1.0).abs() < 0.02,
            "channel {c}: far lit side holds {:.4} of the diffuse value",
            energy[c]
        );
        assert!(
            mean_err[c] < 0.008,
            "channel {c}: mean bin error {:.4}",
            mean_err[c]
        );
        assert!(worst[c] < 0.03, "channel {c}: worst bin error {:.4}", worst[c]);
    }
    // The wider profile carries more light into the shadow: blue (d = 4 cm) reaches furthest.
    let band: Vec<usize> = (0..means.len())
        .filter(|&x| s[x] < -0.02 && s[x] > -0.06)
        .collect();
    let into_shadow =
        [0, 1, 2].map(|c| band.iter().map(|&x| means[x][c] / flat[c]).sum::<f64>() / band.len() as f64);
    assert!(
        into_shadow[2] > into_shadow[1] && into_shadow[1] > into_shadow[0],
        "{into_shadow:?}"
    );
}

#[test]
fn without_scattering_the_edge_stays_hard() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let settings = PtSettings {
        spp: 8,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    // tint 0: the subsurface entry is dropped at pack time, the plate is plainly diffuse.
    let img = tracer.render(&scene(false, 0.0), &settings).expect("trace");
    let (means, s) = columns(&img);
    let shadowed: f64 = means
        .iter()
        .zip(&s)
        .filter(|(_, v)| **v < -0.01)
        .map(|(m, _)| m[0])
        .sum();
    assert_eq!(shadowed, 0.0, "light past a hard shadow edge without scattering");
    // And with scattering the shadow side receives light (the test above holds how much).
    let img = tracer.render(&scene(false, 1.0), &settings).expect("trace");
    let (means, s) = columns(&img);
    let shadowed: f64 = means
        .iter()
        .zip(&s)
        .filter(|(_, v)| **v < -0.01 && **v > -0.05)
        .map(|(m, _)| m[0])
        .sum();
    assert!(shadowed > 0.0);
}
