// SPDX-License-Identifier: MIT-0
//! `combat_shots`: round-7 VFX evidence on DO27's own maps and assets (lane vfx-look).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> combat_shots --out <dir> [--size 960x540] [--only hover,hits,daylight]
//! ```
//!
//! - `hover`: DO27's X12 hovering over Mars dust (The Switchbacks) and Europa ice and frost (Bore
//!   Twelve), plus Earth air on dust, at 0.6, 2.4 and 4 m above the ground; "before" is the hull
//!   with no wash (the previous code had none). `hover_<row>_<col>.png`.
//! - `hover_time`: the Mars and Earth washes at 0.6 m after 0.6, 1.5 and 3.5 s.
//! - `plumes`: DO27's gleaner with its four duct plumes in Earth air, on Mars and airless, low and
//!   high, against the previous generic `exhaust`.
//! - `hits`: rounds on the X12's hull (metal) and canopy (laminate glass, stamped by
//!   glass-damage) and on Bore Twelve's comm tower (concrete), on Europa (airless) and one metal
//!   row on Mars; "before" is the previous generic `impact` with no mark. `hits_<row>_<col>.png`.
//! - `daylight`: muzzle flashes (rifle, chaincannon) and tracers on Europa's bright ice in full
//!   sun with the `fireball_20m` camera, before (previous presets) and after; writes
//!   `daylight_<row>_{none,before,after}.png` and the readability metrics
//!   ([`lookdev::fx::daylight_read`]).
//!
//! Every shot also writes the frame without its effect (`_none`) and `metrics.txt`.

use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::write_png;
use lookdev::fx::{FxDecal, FxInsert, FxSpawn, daylight_read};
use lookdev::hero::{self, HeroAsset, HeroModel};
use lookdev::look_r2::{HeroInsert, R2Options, R2Planets, R2Shot, render_hero_fx};
use regia_core::glam::{Mat4, Vec3};
use regia_render::LabMesh;
use regia_render::glass::{GlassCrackSets, GlassDamage, GlassView, apply_one};
use regia_vfx::combat::{Calibre, HitSurface, combat_effect_in, hit_decals};
use regia_vfx::format::{BlendMode, VfxSystemDef};
use regia_vfx::hover::{HOVER_REFERENCE_RADIUS, HoverSurface, hover_intensity, hover_wash_in};
use regia_vfx::medium::{EUROPA_GRAVITY, VfxMedium};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("combat_shots: {err}");
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

/// Rendered frames for a capture `age` seconds in: ~15 simulation steps a second (60 for
/// sub-second captures), at least 16 frames of convergence.
fn frames_for(age: f32) -> u32 {
    let rate = if age < 1.0 { 60.0 } else { 15.0 };
    ((age * rate).ceil() as u32).max(16)
}

struct Ctx {
    device: regia_render::RenderDevice,
    planets: R2Planets,
    options: R2Options,
    out: PathBuf,
    lines: Vec<String>,
}

impl Ctx {
    fn save(&mut self, name: &str, px: &[u8]) -> Result<PathBuf, LookdevError> {
        let path = self.out.join(format!("{name}.png"));
        write_png(&path, self.options.width, self.options.height, px)?;
        println!("{}", path.display());
        Ok(path)
    }

    /// Renders `hero` with a scripted effect captured at `age`.
    #[allow(clippy::too_many_arguments)]
    fn shoot(
        &mut self,
        shot: R2Shot,
        hero: &HeroInsert,
        medium: VfxMedium,
        age: f32,
        defs: Vec<VfxSystemDef>,
        spawns: Vec<FxSpawn>,
        decals: Vec<FxDecal>,
    ) -> Result<Vec<u8>, LookdevError> {
        let mut options = self.options;
        options.frames = frames_for(age);
        let mut fx = FxInsert::scripted(&self.device, options.frames, medium, age, defs, spawns, decals)?;
        let (px, _) = render_hero_fx(&self.device, &mut self.planets, shot, options, hero, &mut fx)?;
        Ok(px)
    }
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
    options.width = 960;
    options.height = 540;
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        options.width = w.parse().unwrap_or(options.width);
        options.height = h.parse().unwrap_or(options.height);
    }
    let only: Option<Vec<&str>> = value(args, "--only").map(|v| v.split(',').collect());
    let wants = |tag: &str| only.as_ref().is_none_or(|o| o.contains(&tag));
    let assets = hero::catalog(&content);
    let find = |name: &str| {
        assets
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| LookdevError::Config(format!("no hero asset {name}")))
    };
    let mut ctx = Ctx {
        device: regia_terrain::gpu::headless_bcn_device()?,
        planets: R2Planets::default(),
        options,
        out,
        lines: Vec::new(),
    };
    if wants("hover") {
        hover(&mut ctx, &find("x12")?)?;
    }
    if wants("hover_time") {
        hover_time(&mut ctx, &find("x12")?)?;
    }
    if wants("plumes") {
        let gleaner = HeroAsset {
            name: "gleaner".into(),
            files: vec![content.join("units/gleaner/SM_Scavenger.gltf")],
            planet: lookdev::Planet::Mars,
        };
        plumes(&mut ctx, &gleaner)?;
    }
    if wants("hits") {
        hits(&mut ctx, &find("x12")?, &find("commtower_a52")?)?;
    }
    if wants("daylight") {
        daylight(&mut ctx)?;
    }
    let path = ctx.out.join("metrics.txt");
    std::fs::write(&path, ctx.lines.join("\n") + "\n").map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// The DO27 plate camera's look-at point on `shot`'s map, on the ground.
fn anchor(ctx: &mut Ctx, shot: R2Shot) -> Result<Vec3, LookdevError> {
    let terrain = ctx.planets.get(shot.planet(), ctx.options.terrain_resolution)?;
    let at = terrain
        .do27
        .as_deref()
        .and_then(|m| m.cameras.get(shot.name()))
        .map_or(Vec3::ZERO, |c| Vec3::from_array(c.target));
    Ok(Vec3::new(at.x, terrain.height_at(at.x, at.z), at.z))
}

fn ground_at(ctx: &mut Ctx, shot: R2Shot, x: f32, z: f32) -> Result<f32, LookdevError> {
    Ok(ctx
        .planets
        .get(shot.planet(), ctx.options.terrain_resolution)?
        .height_at(x, z))
}

/// `asset` loaded and placed with its footprint centred on `at` (x, z), its underside `lift`
/// metres above `at.y`, turned `yaw`.
fn placed(asset: &HeroAsset, at: Vec3, lift: f32, yaw: f32) -> Result<HeroModel, LookdevError> {
    let mut model = hero::load(asset, 1024)?;
    hero::place(&mut model, yaw, Vec3::ZERO);
    let centre = (model.min + model.max) * 0.5;
    let offset = Vec3::new(at.x - centre.x, at.y - model.min.y + lift, at.z - centre.z);
    hero::place(&mut model, 0.0, offset);
    Ok(model)
}

fn insert(model: &HeroModel, eye: Vec3, target: Vec3, fov_deg: f32) -> HeroInsert {
    HeroInsert {
        meshes: model.meshes.clone(),
        materials: model.materials.clone(),
        eye: eye.to_array(),
        target: target.to_array(),
        fov_y: fov_deg.to_radians(),
        glass: Vec::new(),
        occluders: Vec::new(),
        lights: Vec::new(),
    }
}

/// Every effect of a hover row: the wash in its medium (the scene resolves the variant).
fn hover_defs(surface: HoverSurface, medium: &VfxMedium) -> Vec<VfxSystemDef> {
    let mut defs = vec![hover_wash_in(surface, &VfxMedium::earth())];
    if medium.variant_suffix().is_some() {
        defs.push(hover_wash_in(surface, medium));
    }
    defs
}

fn hover(ctx: &mut Ctx, x12: &HeroAsset) -> Result<(), LookdevError> {
    let rows: [(&str, R2Shot, VfxMedium, HoverSurface); 4] = [
        (
            "mars_dust",
            R2Shot::SwitchbacksMid,
            VfxMedium::mars(),
            HoverSurface::Dust,
        ),
        (
            "airless_ice",
            R2Shot::Fireball20m,
            VfxMedium::airless(EUROPA_GRAVITY),
            HoverSurface::Ice,
        ),
        (
            "airless_frost",
            R2Shot::Fireball20m,
            VfxMedium::airless(EUROPA_GRAVITY),
            HoverSurface::Frost,
        ),
        (
            "earth_dust",
            R2Shot::SwitchbacksMid,
            VfxMedium::earth(),
            HoverSurface::Dust,
        ),
    ];
    let age = 3.5;
    for (row, shot, medium, surface) in rows {
        let at = anchor(ctx, shot)?;
        let base = surface.effect().unwrap_or("hover_wash_dust");
        for (col, height) in [("before", 0.6f32), ("h0.6", 0.6), ("h2.4", 2.4), ("h4.0", 4.0)] {
            let model = placed(x12, at, height, 0.5)?;
            let half = (model.max - model.min) * 0.5;
            let radius = half.x.max(half.z);
            let centre = (model.min + model.max) * 0.5;
            let ground = Vec3::new(centre.x, ground_at(ctx, shot, centre.x, centre.z)?, centre.z);
            // One camera per row (the hull rises in a fixed frame): three-quarter view from 13 m,
            // 3.5 m up, aimed 1.6 m above the ground under the hull.
            let dir = Vec3::new(0.8, 0.0, 0.6).normalize();
            let eye = ground + dir * 13.0 + Vec3::Y * 3.5;
            let target = ground + Vec3::Y * 1.6;
            let hero = insert(&model, eye, target, 44.0);
            let spawns = if col == "before" {
                Vec::new()
            } else {
                vec![FxSpawn {
                    scale: radius / HOVER_REFERENCE_RADIUS,
                    intensity: hover_intensity(height, radius),
                    ..FxSpawn::new(base, 0.0, ground, Vec3::Y)
                }]
            };
            let px = ctx.shoot(
                shot,
                &hero,
                medium,
                age,
                hover_defs(surface, &medium),
                spawns,
                Vec::new(),
            )?;
            ctx.save(&format!("hover_{row}_{col}"), &px)?;
            ctx.lines.push(format!(
                "hover {row} {col}: footprint {radius:.2} m, intensity {:.3}, hull {:?}..{:?}, eye {eye:?}, target {target:?}",
                hover_intensity(height, radius),
                model.min,
                model.max
            ));
        }
    }
    Ok(())
}

/// The Mars and Earth washes over time at the hover height (0.6 m): the outflow rolling out from
/// under the hull at 0.6 s, the roll-up lifting at 1.5 s, the cloud drifting at 3.5 s. Same
/// camera as [`hover`]. `hover_time_<row>_<age>.png`.
fn hover_time(ctx: &mut Ctx, x12: &HeroAsset) -> Result<(), LookdevError> {
    let rows: [(&str, R2Shot, VfxMedium); 2] = [
        ("mars_dust", R2Shot::SwitchbacksMid, VfxMedium::mars()),
        ("earth_dust", R2Shot::SwitchbacksMid, VfxMedium::earth()),
    ];
    let height = 0.6;
    for (row, shot, medium) in rows {
        let at = anchor(ctx, shot)?;
        let model = placed(x12, at, height, 0.5)?;
        let half = (model.max - model.min) * 0.5;
        let radius = half.x.max(half.z);
        let centre = (model.min + model.max) * 0.5;
        let ground = Vec3::new(centre.x, ground_at(ctx, shot, centre.x, centre.z)?, centre.z);
        let dir = Vec3::new(0.8, 0.0, 0.6).normalize();
        let eye = ground + dir * 13.0 + Vec3::Y * 3.5;
        let target = ground + Vec3::Y * 1.6;
        let hero = insert(&model, eye, target, 44.0);
        for age in [0.6f32, 1.5, 3.5] {
            let spawns = vec![FxSpawn {
                scale: radius / HOVER_REFERENCE_RADIUS,
                intensity: hover_intensity(height, radius),
                ..FxSpawn::new("hover_wash_dust", 0.0, ground, Vec3::Y)
            }];
            let px = ctx.shoot(
                shot,
                &hero,
                medium,
                age,
                hover_defs(HoverSurface::Dust, &medium),
                spawns,
                Vec::new(),
            )?;
            ctx.save(&format!("hover_time_{row}_{age}"), &px)?;
        }
    }
    Ok(())
}

/// DO27's gleaner with its four lift ducts (`socket_duct_*`, exhaust down) in Earth air, on Mars
/// and airless, low (underside 1 m up: the airless glow lights the ground) and high (6 m).
/// "Before" is the previous generic `exhaust` plume at the same ducts.
/// `plumes_<row>_<col>.png`.
fn plumes(ctx: &mut Ctx, gleaner: &HeroAsset) -> Result<(), LookdevError> {
    use regia_vfx::plume::{DO27_THRUSTERS, PlumeKind, plume_in};
    // The rem gleaner's duct sockets (manifest `pos_gltf`); the hero loader mirrors glTF X.
    let sockets = [
        Vec3::new(-3.2552, 0.75, 0.25),
        Vec3::new(-3.0133, 0.75, -1.95),
        Vec3::new(3.2552, 0.75, 0.25),
        Vec3::new(3.0133, 0.75, -1.95),
    ];
    let unit = DO27_THRUSTERS
        .iter()
        .find(|t| t.unit == "gleaner")
        .ok_or_else(|| LookdevError::Config("no gleaner in DO27_THRUSTERS".into()))?;
    let rows: [(&str, R2Shot, VfxMedium); 3] = [
        ("earth", R2Shot::SwitchbacksMid, VfxMedium::earth()),
        ("mars", R2Shot::SwitchbacksMid, VfxMedium::mars()),
        ("airless", R2Shot::Fireball20m, VfxMedium::airless(EUROPA_GRAVITY)),
    ];
    let yaw = 0.5f32;
    for (row, shot, medium) in rows {
        let at = anchor(ctx, shot)?;
        let mut defs = vec![
            regia_vfx::showcase::exhaust(),
            plume_in(unit.kind, &VfxMedium::earth()),
        ];
        if medium.variant_suffix().is_some() {
            defs.push(plume_in(unit.kind, &medium));
        }
        for (col, height, after) in [("before", 1.0f32, false), ("low", 1.0, true), ("high", 6.0, true)] {
            let mut model = hero::load(gleaner, 1024)?;
            hero::place(&mut model, yaw, Vec3::ZERO);
            let centre = (model.min + model.max) * 0.5;
            let offset = Vec3::new(at.x - centre.x, at.y - model.min.y + height, at.z - centre.z);
            hero::place(&mut model, 0.0, offset);
            let place = Mat4::from_translation(offset) * Mat4::from_rotation_y(yaw);
            let centre = (model.min + model.max) * 0.5;
            let ground = Vec3::new(centre.x, ground_at(ctx, shot, centre.x, centre.z)?, centre.z);
            let dir = Vec3::new(0.8, 0.0, 0.6).normalize();
            let eye = ground + dir * 21.0 + Vec3::Y * 4.5;
            let target = ground + Vec3::Y * 3.0;
            let hero = insert(&model, eye, target, 46.0);
            let down = Vec3::from(unit.exhaust);
            let spawns: Vec<FxSpawn> = sockets
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let p = place.transform_point3(Vec3::new(-s.x, s.y, s.z));
                    let d = place.transform_vector3(down).normalize_or(Vec3::NEG_Y);
                    if after {
                        FxSpawn {
                            seed: 40 + i as u64,
                            ..FxSpawn::new(PlumeKind::Duct.effect(), 0.0, p, d)
                        }
                    } else {
                        // The showcase exhaust blows along local -Z: forward = -exhaust.
                        FxSpawn {
                            seed: 40 + i as u64,
                            ..FxSpawn::new("exhaust", 0.0, p, -d)
                        }
                    }
                })
                .collect();
            let px = ctx.shoot(shot, &hero, medium, 2.5, defs.clone(), spawns, Vec::new())?;
            ctx.save(&format!("plumes_{row}_{col}"), &px)?;
        }
    }
    Ok(())
}

/// Meshes of `model` whose material transmits (the canopy).
fn glass_meshes(model: &HeroModel) -> Vec<usize> {
    (0..model.meshes.len())
        .filter(|&i| model.materials[model.meshes[i].material].transmission > 0.0)
        .collect()
}

fn merged(model: &HeroModel, which: &[usize]) -> LabMesh {
    let mut out = LabMesh {
        material: model.meshes[which[0]].material,
        ..LabMesh::unskinned()
    };
    for &i in which {
        let m = &model.meshes[i];
        let base = out.vertices.len() as u32;
        out.vertices.extend_from_slice(&m.vertices);
        out.indices.extend(m.indices.iter().map(|k| k + base));
    }
    out
}

/// The effects every hit row may play: the previous `impact` and the hits, in `medium`.
fn hit_defs(medium: &VfxMedium) -> Vec<VfxSystemDef> {
    let mut defs = vec![regia_vfx::showcase::impact()];
    for surface in HitSurface::ALL {
        if let Some(d) = combat_effect_in(surface.effect(), &VfxMedium::earth()) {
            defs.push(d);
        }
        if medium.variant_suffix().is_some()
            && let Some(d) = combat_effect_in(surface.effect(), medium)
        {
            defs.push(d);
        }
    }
    defs
}

#[allow(clippy::too_many_lines)] // three surfaces, one structure
fn hits(ctx: &mut Ctx, x12: &HeroAsset, tower: &HeroAsset) -> Result<(), LookdevError> {
    let europa = VfxMedium::airless(EUROPA_GRAVITY);
    let mars = VfxMedium::mars();
    let cols: [(&str, bool, f32); 5] = [
        ("before_0.05", false, 0.05),
        ("after_0.05", true, 0.05),
        ("before_0.3", false, 0.3),
        ("after_0.3", true, 0.3),
        ("after_1.5", true, 1.5),
    ];
    let t_hit = 0.1;
    // Rows: (name, shot, medium, surface, asset, view direction from the hit, calibre).
    struct Row<'a> {
        name: &'a str,
        shot: R2Shot,
        medium: VfxMedium,
        surface: HitSurface,
        asset: &'a HeroAsset,
        calibre: Calibre,
    }
    let rows = [
        Row {
            name: "metal_airless",
            shot: R2Shot::Fireball20m,
            medium: europa,
            surface: HitSurface::Metal,
            asset: x12,
            calibre: Calibre::Medium,
        },
        Row {
            name: "glass_airless",
            shot: R2Shot::Fireball20m,
            medium: europa,
            surface: HitSurface::Glass,
            asset: x12,
            calibre: Calibre::Medium,
        },
        Row {
            name: "concrete_airless",
            shot: R2Shot::Fireball20m,
            medium: europa,
            surface: HitSurface::Concrete,
            asset: tower,
            calibre: Calibre::Medium,
        },
        Row {
            name: "metal_mars",
            shot: R2Shot::SwitchbacksMid,
            medium: mars,
            surface: HitSurface::Metal,
            asset: x12,
            calibre: Calibre::Medium,
        },
    ];
    for row in rows {
        let at = anchor(ctx, row.shot)?;
        let model = placed(row.asset, at, 0.0, 0.5)?;
        let centre = (model.min + model.max) * 0.5;
        let size = (model.max - model.min).max_element();
        // Aim at the hull side, the canopy, or the tower wall, from the sun-lit side.
        let glass_idx = glass_meshes(&model);
        let solid: Vec<LabMesh> = model
            .meshes
            .iter()
            .enumerate()
            .filter(|(i, _)| !glass_idx.contains(i))
            .map(|(_, m)| m.clone())
            .collect();
        let dir = Vec3::new(0.8, 0.25, 0.55).normalize();
        let (target_mesh, aim) = match row.surface {
            HitSurface::Glass => {
                if glass_idx.is_empty() {
                    return Err(LookdevError::Config("the X12 has no canopy glass".into()));
                }
                let g = merged(&model, &glass_idx);
                let (lo, hi) = bounds(&g);
                (vec![g], (lo + hi) * 0.5)
            }
            HitSurface::Concrete => (solid.clone(), centre - Vec3::Y * (size * 0.2)),
            HitSurface::Metal => (solid.clone(), centre - Vec3::Y * (size * 0.08)),
        };
        let probe = aim + dir * (size * 2.0);
        let Some((point, normal)) = hero::raycast(&target_mesh, probe, (aim - probe).normalize()) else {
            return Err(LookdevError::Config(format!(
                "{}: the probe ray missed",
                row.name
            )));
        };
        let reach = match row.calibre {
            Calibre::Light => 2.2,
            Calibre::Medium => 2.8,
            Calibre::Heavy => 3.6,
        };
        let eye = point + (normal * 0.7 + dir * 0.3 + Vec3::Y * 0.25).normalize() * reach;
        for (col, after, age) in cols {
            let mut hero = insert(&model, eye, point, 40.0);
            let mut decals = Vec::new();
            let effect = if after { row.surface.effect() } else { "impact" };
            if after {
                for (decal, surface) in hit_decals(row.surface, row.calibre, point, normal, 11) {
                    decals.push(FxDecal {
                        at: t_hit,
                        decal,
                        surface,
                    });
                }
                if row.surface == HitSurface::Glass {
                    // glass-damage stamps the round's crack into the canopy's page.
                    let sets = GlassCrackSets::default();
                    let mut merged_insert = hero.meshes.clone();
                    let glass_mesh = merged(&model, &glass_idx);
                    merged_insert.retain(|m| model.materials[m.material].transmission <= 0.0);
                    merged_insert.push(glass_mesh.clone());
                    hero.meshes = merged_insert;
                    let mut damage = GlassDamage::new("laminate");
                    damage.impact(point, row.calibre.glass_energy());
                    if let Some(set) = sets.get("laminate") {
                        apply_one(
                            &mut damage,
                            set,
                            &glass_mesh.vertices,
                            &glass_mesh.indices,
                            1,
                            Mat4::IDENTITY,
                        );
                    }
                    hero.glass = vec![(hero.meshes.len() - 1, GlassView::default(), Some(damage))];
                }
            }
            let spawns = vec![FxSpawn {
                scale: if after { row.calibre.scale() } else { 1.0 },
                ..FxSpawn::new(effect, t_hit, point, normal)
            }];
            let px = ctx.shoot(
                row.shot,
                &hero,
                row.medium,
                t_hit + age,
                hit_defs(&row.medium),
                spawns,
                decals,
            )?;
            ctx.save(&format!("hits_{}_{col}", row.name), &px)?;
        }
        ctx.lines
            .push(format!("hits {}: point {point:?} normal {normal:?}", row.name));
    }
    Ok(())
}

fn bounds(mesh: &LabMesh) -> (Vec3, Vec3) {
    mesh.vertices
        .iter()
        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), v| {
            let p = Vec3::from_array(v.position);
            (lo.min(p), hi.max(p))
        })
}

/// The previous code's muzzle flash and tracer (before this round's daylight pass): a big
/// additive flare and bore at emissive 5, no tongues; an additive `#ff8a22` tracer core at
/// emissive 4.
fn legacy(def: &VfxSystemDef) -> VfxSystemDef {
    let mut def = def.clone();
    let muzzle = def.emitters.iter().any(|e| e.name == "flare");
    let cannon = def.name.starts_with("muzzle_cannon");
    def.name = format!("{}_legacy", def.name);
    if muzzle {
        // The previous muzzle: an additive flare and bore at emissive 5, no core, no tongues
        // (the cannon is that muzzle at the cannon's flare size).
        def.emitters.retain(|e| e.name != "tongues" && e.name != "core");
        for e in &mut def.emitters {
            match e.name.as_str() {
                "flare" => {
                    e.init.size = if cannon {
                        regia_vfx::format::RangeDef { min: 1.1, max: 1.4 }
                    } else {
                        regia_vfx::format::RangeDef { min: 1.35, max: 1.65 }
                    };
                    e.init.color[0].color = "#fff4d4".into();
                    e.render.emissive = 5.0;
                }
                "flash" => e.render.emissive = 5.0,
                _ => {}
            }
        }
    } else {
        // The previous tracer: an additive `#ff8a22` core at emissive 4.
        for e in &mut def.emitters {
            e.render.blend = BlendMode::Additive;
            e.render.emissive = 4.0;
            e.init.color[0].color = "#ff8a22".into();
        }
    }
    def
}

#[allow(clippy::too_many_lines)] // three rows of two variants
fn daylight(ctx: &mut Ctx) -> Result<(), LookdevError> {
    let shot = R2Shot::Fireball20m;
    let medium = VfxMedium::airless(EUROPA_GRAVITY);
    let at = anchor(ctx, shot)?;
    let terrain = ctx.planets.get(shot.planet(), ctx.options.terrain_resolution)?;
    let cam = terrain
        .do27
        .as_deref()
        .and_then(|m| m.cameras.get(shot.name()).copied());
    let (eye, target) = cam.map_or((at + Vec3::new(0.0, 6.0, 20.0), at), |c| {
        (Vec3::from_array(c.eye), Vec3::from_array(c.target))
    });
    let look = (target - eye).normalize();
    let right = look.cross(Vec3::Y).normalize();
    // The fire direction runs across the frame, a little away from the camera.
    let fire = (right + look * 0.35).normalize();
    let muzzle_at = at - right * 5.0 + Vec3::Y * 1.6;
    let mut base_defs: Vec<VfxSystemDef> = Vec::new();
    for name in ["muzzle_flash", "muzzle_cannon", "tracer", "tracer_heavy"] {
        if let Some(d) = combat_effect_in(name, &VfxMedium::earth()) {
            base_defs.push(legacy(&d));
            base_defs.push(d);
        }
        if let Some(d) = combat_effect_in(name, &medium)
            && d.name.contains('@')
        {
            base_defs.push(d);
        }
    }
    // (row, effect, repeat period, capture age, aim)
    let rows: [(&str, &str, Option<f32>, f32, Vec3); 4] = [
        ("rifle_muzzle", "muzzle_flash", Some(0.1), 0.53, fire),
        ("cannon_muzzle", "muzzle_cannon", Some(0.15), 0.48, fire),
        (
            "tracer_ice",
            "tracer",
            None,
            0.7,
            (fire - Vec3::Y * 0.05).normalize(),
        ),
        (
            "tracer_sky",
            "tracer_heavy",
            None,
            0.8,
            (fire + Vec3::Y * 0.32).normalize(),
        ),
    ];
    let hero = HeroInsert {
        meshes: Vec::new(),
        materials: Vec::new(),
        eye: eye.to_array(),
        target: target.to_array(),
        fov_y: cam.map_or(60f32.to_radians(), |c| {
            c.fov_y(ctx.options.width as f32 / ctx.options.height.max(1) as f32)
        }),
        glass: Vec::new(),
        occluders: Vec::new(),
        lights: Vec::new(),
    };
    for (row, effect, repeat, age, aim) in rows {
        let mut frames = Vec::new();
        for label in ["none", "before", "after"] {
            let spawns = match label {
                "none" => Vec::new(),
                _ => {
                    let name = if label == "before" {
                        format!("{effect}_legacy")
                    } else {
                        effect.to_string()
                    };
                    vec![FxSpawn {
                        repeat,
                        ..FxSpawn::new(&name, 0.0, muzzle_at, -aim)
                    }]
                }
            };
            let px = ctx.shoot(shot, &hero, medium, age, base_defs.clone(), spawns, Vec::new())?;
            ctx.save(&format!("daylight_{row}_{label}"), &px)?;
            frames.push(px);
        }
        for (label, i) in [("before", 1usize), ("after", 2)] {
            let m = daylight_read(&frames[i], &frames[0], ctx.options.width, ctx.options.height);
            let line = format!(
                "daylight {row} {label}: covers {} px, clipped {} px ({:.1} %), chroma contrast {:.3}, luma contrast {:.3}, saturation {:.3}",
                m.covered,
                m.clipped,
                100.0 * m.clipped as f32 / m.covered.max(1) as f32,
                m.chroma_contrast,
                m.luma_contrast,
                m.saturation
            );
            println!("{line}");
            ctx.lines.push(line);
        }
    }
    Ok(())
}
