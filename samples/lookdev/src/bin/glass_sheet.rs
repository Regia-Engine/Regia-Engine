// SPDX-License-Identifier: MIT-0
//! `glass_sheet`: DO27 glass damage evidence (lane glass-damage). A unit canopy (laminate) and a
//! building window (pressure pane) at D0, D1, D2 and D3 from outside, plus the canopy from the
//! pilot's seat (`GlassView::viewer_inside`) intact and at D2, on DO27's own maps.
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> glass_sheet --out <dir> [--size 960x540] [--frames 10]
//!     [--vehicle x12] [--building commtower_rem]
//! ```
//!
//! DO27's buildings have no glazing in the content drop, so the window is a framed pane in the
//! building's glass material standing beside it on the map.
//!
//! Writes `<dir>/{canopy,window}_d{0..3}.png`, `<dir>/inside_d{0,2}.png` and `<dir>/sheet.png`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::write_png;
use lookdev::hero::{self, HeroAsset};
use lookdev::look_r2::{HeroInsert, R2Options, R2Planets, R2Shot, render_hero};
use regia_core::glam::{Mat4, Vec3};
use regia_render::LabMesh;
use regia_render::glass::{GlassCrackSets, GlassDamage, GlassView, apply_one};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("glass_sheet: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Moves every transmissive mesh into one mesh (one crack page across the whole canopy or
/// window set), returning its index.
fn merge_glass(insert: &mut HeroInsert) -> Option<usize> {
    let glass: Vec<usize> = (0..insert.meshes.len())
        .filter(|&i| insert.materials[insert.meshes[i].material].transmission > 0.0)
        .collect();
    let first = *glass.first()?;
    let mut merged = LabMesh {
        material: insert.meshes[first].material,
        ..LabMesh::unskinned()
    };
    for &i in &glass {
        let m = &insert.meshes[i];
        let base = merged.vertices.len() as u32;
        merged.vertices.extend_from_slice(&m.vertices);
        merged.indices.extend(m.indices.iter().map(|k| k + base));
    }
    let mut kept: Vec<LabMesh> = insert
        .meshes
        .drain(..)
        .enumerate()
        .filter(|(i, _)| !glass.contains(i))
        .map(|(_, m)| m)
        .collect();
    kept.push(merged);
    insert.meshes = kept;
    Some(insert.meshes.len() - 1)
}

/// World hits on `glass` toward `targets` from `eye`.
fn hits(glass: &LabMesh, eye: Vec3, targets: &[Vec3]) -> Vec<Vec3> {
    targets
        .iter()
        .filter_map(|t| {
            hero::raycast(std::slice::from_ref(glass), eye, (*t - eye).normalize()).map(|(p, _)| p)
        })
        .collect()
}

/// A damage state: the cumulative impacts up to `level` (each level adds its own hits).
fn damage_at(set: &str, glass: &LabMesh, level: u8, hits_by_level: &[Vec<Vec3>; 3]) -> Option<GlassDamage> {
    if level == 0 {
        return None;
    }
    let sets = GlassCrackSets::default();
    let cooked = sets.get(set)?;
    let mut d = GlassDamage::new(set);
    for l in 1..=level {
        for p in &hits_by_level[usize::from(l - 1)] {
            // D3 laminate: only the hardest hits punch through.
            let energy = if l == 3 { 6.0 } else { 1.0 };
            d.impact_at_level(*p, energy, l, 0.0);
        }
    }
    apply_one(&mut d, cooked, &glass.vertices, &glass.indices, 1, Mat4::IDENTITY);
    Some(d)
}

struct Subject {
    insert: HeroInsert,
    glass: usize,
    shot: R2Shot,
}

fn load_subject(
    asset: &HeroAsset,
    planets: &mut R2Planets,
    options: R2Options,
    yaw: f32,
) -> Result<Subject, LookdevError> {
    let mut model = hero::load(asset, 1024)?;
    let shot = match asset.planet {
        lookdev::Planet::Mars => R2Shot::SwitchbacksMid,
        lookdev::Planet::Europa => R2Shot::BoreTwelveMid,
    };
    let terrain = planets.get(asset.planet, options.terrain_resolution)?;
    let anchor = terrain
        .do27
        .as_deref()
        .and_then(|m| m.cameras.get(shot.name()))
        .map_or(Vec3::ZERO, |c| Vec3::from_array(c.target));
    let ground = terrain.height_at(anchor.x, anchor.z);
    let centre = (model.min + model.max) * 0.5;
    let offset = Vec3::new(
        anchor.x - centre.x,
        ground - model.min.y - 0.05,
        anchor.z - centre.z,
    );
    hero::place(&mut model, yaw, offset);
    let mut insert = HeroInsert {
        meshes: std::mem::take(&mut model.meshes),
        materials: std::mem::take(&mut model.materials),
        eye: [0.0; 3],
        target: [0.0; 3],
        fov_y: 0.7,
        glass: Vec::new(),
        occluders: Vec::new(),
        lights: Vec::new(),
    };
    let glass = merge_glass(&mut insert)
        .ok_or_else(|| LookdevError::Config(format!("{} has no transmissive glass", asset.name)))?;
    Ok(Subject { insert, glass, shot })
}

/// A box of half extents `half` centred at `centre`.
fn box_at(half: [f32; 3], centre: Vec3, material: usize) -> LabMesh {
    let (mut vertices, indices) = regia_render::box_mesh(half);
    for v in &mut vertices {
        v.position = (Vec3::from_array(v.position) + centre).to_array();
    }
    LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    }
}

fn bounds(mesh: &LabMesh) -> (Vec3, Vec3) {
    mesh.vertices
        .iter()
        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), v| {
            let p = Vec3::from_array(v.position);
            (lo.min(p), hi.max(p))
        })
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let out = PathBuf::from(
        value(args, "--out").ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?,
    );
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    let mut options = R2Options::evidence();
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    options.frames = value(args, "--frames").and_then(|v| v.parse().ok()).unwrap_or(10);
    let vehicle_name = value(args, "--vehicle").unwrap_or("x12");
    let building_name = value(args, "--building").unwrap_or("commtower_rem");
    let assets = hero::catalog(&content);
    let find = |name: &str| {
        assets
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| LookdevError::Config(format!("no hero asset {name}")))
    };
    let device = regia_terrain::gpu::headless_bcn_device()?;
    let mut planets = R2Planets::default();
    let mut tiles: Vec<(String, Vec<u8>)> = Vec::new();

    // ---- Canopy (laminate) ----
    let mut canopy = load_subject(&find(vehicle_name)?, &mut planets, options, 0.6)?;
    let (lo, hi) = bounds(&canopy.insert.meshes[canopy.glass]);
    let c = (lo + hi) * 0.5;
    let size = (hi - lo).max_element().max(0.5);
    // Outside: three-quarter front, a little above the canopy.
    let front = Vec3::new(0.6f32.sin(), 0.0, 0.6f32.cos());
    let side = Vec3::Y.cross(front).normalize();
    let eye_out = c + front * (size * 1.3 + 0.5) + side * (size * 0.6) + Vec3::Y * (size * 0.45);
    // Impact targets spread over the canopy as seen from that camera.
    let spread = |k: usize, r: f32| {
        let a = k as f32 * 2.399;
        c + side * (a.cos() * r * size * 0.35) + Vec3::Y * (a.sin() * r * size * 0.2)
    };
    let glass_mesh = canopy.insert.meshes[canopy.glass].clone();
    let canopy_hits = [
        hits(&glass_mesh, eye_out, &[spread(0, 0.3), spread(1, 0.8)]),
        hits(
            &glass_mesh,
            eye_out,
            &[spread(2, 0.5), spread(3, 0.9), spread(4, 0.2)],
        ),
        hits(
            &glass_mesh,
            eye_out,
            &[spread(5, 0.4), spread(6, 0.7), spread(7, 0.1)],
        ),
    ];
    eprintln!(
        "canopy hits per level: {:?}",
        canopy_hits.iter().map(Vec::len).collect::<Vec<_>>()
    );
    for level in 0..=3u8 {
        canopy.insert.glass = vec![(
            canopy.glass,
            GlassView::default(),
            damage_at("laminate", &glass_mesh, level, &canopy_hits),
        )];
        canopy.insert.eye = eye_out.to_array();
        canopy.insert.target = c.to_array();
        canopy.insert.fov_y = 30f32.to_radians();
        let (px, _) = render_hero(&device, &mut planets, canopy.shot, options, &canopy.insert)?;
        save(&out, &format!("canopy_d{level}"), options, &px, &mut tiles)?;
    }
    // Inside: the pilot's eye behind the canopy, looking forward through it. The D2 view adds
    // hits on the glass that this eye sees (forward and overhead), so the cracks are in frame.
    let eye_in = c - front * (size * 0.15) - Vec3::Y * (size * 0.1);
    let ahead = |s: f32, u: f32| eye_in + front * 3.0 + side * s + Vec3::Y * u;
    let inside_hits = hits(
        &glass_mesh,
        eye_in,
        &[
            ahead(-0.8, 1.2),
            ahead(0.7, 0.9),
            ahead(0.1, 2.0),
            ahead(-0.2, 0.5),
        ],
    );
    eprintln!("inside hits: {}", inside_hits.len());
    let mut seen_hits = canopy_hits.clone();
    seen_hits[1].extend(inside_hits);
    for level in [0u8, 2] {
        canopy.insert.glass = vec![(
            canopy.glass,
            GlassView::viewer_inside(),
            damage_at("laminate", &glass_mesh, level, &seen_hits),
        )];
        canopy.insert.eye = eye_in.to_array();
        canopy.insert.target = (eye_in + front * 10.0 + Vec3::Y * 0.5).to_array();
        canopy.insert.fov_y = 70f32.to_radians();
        let (px, _) = render_hero(&device, &mut planets, canopy.shot, options, &canopy.insert)?;
        save(&out, &format!("inside_d{level}"), options, &px, &mut tiles)?;
    }

    // ---- Building window (pressure pane) ----
    // DO27's buildings carry no glazing in the content drop (the commtower's glass is a 10 cm
    // decorative hoop), so the window is a framed 1.6 × 1.2 m pane in the building's own glass
    // material, standing on the map beside the building.
    let mut building = load_subject(&find(building_name)?, &mut planets, options, 0.0)?;
    let glass_material = building.insert.meshes[building.glass].material;
    let frame_material = building
        .insert
        .meshes
        .iter()
        .map(|m| m.material)
        .find(|&m| m != glass_material)
        .unwrap_or(glass_material);
    let (tlo, thi) = building.insert.meshes.iter().map(bounds).fold(
        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
        |(a, b), (lo, hi)| (a.min(lo), b.max(hi)),
    );
    let base = Vec3::new(thi.x + 2.5, tlo.y, (tlo.z + thi.z) * 0.5);
    let pane_centre = base + Vec3::Y * 2.0;
    for (half, centre) in [
        ([0.92, 0.06, 0.07], pane_centre + Vec3::Y * 0.66),
        ([0.92, 0.06, 0.07], pane_centre - Vec3::Y * 0.66),
        ([0.06, 1.36, 0.07], base + Vec3::new(-0.86, 1.36, 0.0)),
        ([0.06, 1.36, 0.07], base + Vec3::new(0.86, 1.36, 0.0)),
    ] {
        building.insert.meshes.push(box_at(half, centre, frame_material));
    }
    building
        .insert
        .meshes
        .push(box_at([0.8, 0.6, 0.012], pane_centre, glass_material));
    building.glass = building.insert.meshes.len() - 1;
    let window = building.insert.meshes[building.glass].clone();
    let (wlo, whi) = bounds(&window);
    let wc = (wlo + whi) * 0.5;
    // The glass faces along its thinnest extent, away from the building's centre (DO27's
    // commtower glass is a horizontal disc at the top, so this looks down onto it).
    let (blo, bhi) = building.insert.meshes.iter().map(bounds).fold(
        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
        |(a, b), (lo, hi)| (a.min(lo), b.max(hi)),
    );
    let ext = whi - wlo;
    let axis = if ext.x <= ext.y && ext.x <= ext.z {
        Vec3::X
    } else if ext.y <= ext.z {
        Vec3::Y
    } else {
        Vec3::Z
    };
    let axis = if (wc - (blo + bhi) * 0.5).dot(axis) >= 0.0 {
        axis
    } else {
        -axis
    };
    let t1 = Vec3::Y.cross(axis).normalize_or(Vec3::X);
    let t2 = axis.cross(t1).normalize_or(Vec3::Z);
    let reach = (ext.max_element() * 0.5).min(1.0);
    // Off-centre: the glass may be a ring around a central part (the commtower's spike).
    let (hit, _) = [0.55f32, 0.75, 0.35, 0.0]
        .iter()
        .find_map(|f| {
            let probe = wc + axis * 20.0 + t1 * (reach * f);
            hero::raycast(std::slice::from_ref(&window), probe, -axis)
        })
        .ok_or_else(|| LookdevError::Config("the glass does not face out of the building".into()))?;
    let normal = axis;
    let eye_w = hit + normal * (reach * 3.0) + t1 * (reach * 1.6) + t2 * (reach * 0.4);
    let wspread = |k: usize, r: f32| {
        let a = k as f32 * 2.399;
        hit + t1 * (a.cos() * r * reach) + t2 * (a.sin() * r * reach)
    };
    let window_hits = [
        hits(&window, eye_w, &[wspread(0, 0.45)]),
        hits(&window, eye_w, &[wspread(1, 0.5), wspread(2, 0.6)]),
        hits(&window, eye_w, &[wspread(3, 0.15)]),
    ];
    eprintln!(
        "window hits per level: {:?}",
        window_hits.iter().map(Vec::len).collect::<Vec<_>>()
    );
    for level in 0..=3u8 {
        building.insert.glass = vec![(
            building.glass,
            GlassView::default(),
            damage_at("pressure_pane", &window, level, &window_hits),
        )];
        building.insert.eye = eye_w.to_array();
        building.insert.target = hit.to_array();
        building.insert.fov_y = 40f32.to_radians();
        let (px, _) = render_hero(&device, &mut planets, building.shot, options, &building.insert)?;
        save(&out, &format!("window_d{level}"), options, &px, &mut tiles)?;
    }

    write_sheet(&out.join("sheet.png"), options, &tiles)
}

fn save(
    out: &Path,
    name: &str,
    options: R2Options,
    px: &[u8],
    tiles: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), LookdevError> {
    let path = out.join(format!("{name}.png"));
    write_png(&path, options.width, options.height, px)?;
    println!("{name} -> {}", path.display());
    tiles.push((name.to_string(), px.to_vec()));
    Ok(())
}

/// Tiles at half size, four per row (canopy D0–D3, inside D0/D2, window D0–D3).
fn write_sheet(path: &Path, options: R2Options, tiles: &[(String, Vec<u8>)]) -> Result<(), LookdevError> {
    let (tw, th) = (options.width / 2, options.height / 2);
    let cols = 4u32;
    let rows = (tiles.len() as u32).div_ceil(cols);
    let (w, h) = (tw * cols, th * rows);
    let mut sheet = vec![0u8; (w * h * 4) as usize];
    for (k, (_, px)) in tiles.iter().enumerate() {
        let (ox, oy) = ((k as u32 % cols) * tw, (k as u32 / cols) * th);
        for y in 0..th {
            for x in 0..tw {
                let src = (((y * 2) * options.width + x * 2) * 4) as usize;
                let dst = (((oy + y) * w + ox + x) * 4) as usize;
                sheet[dst..dst + 4].copy_from_slice(&px[src..src + 4]);
            }
        }
    }
    write_png(path, w, h, &sheet)
}
