// SPDX-License-Identifier: MIT-0
//! DO27 effect shots on DO27's own ground (`fireball_20m`, `smoke_80m`): the showcase
//! explosion or smoke column spawned where the plate camera looks, simulated through the
//! frame's translucent layer while [`crate::look_r2`] renders the shot.
//!
//! [`FxInsert::step`] runs once per rendered frame: it advances the CPU scene, uploads it,
//! and returns the effect's particle lights (fire lights the ground and the smoke above it).

use std::sync::{Arc, Mutex};

use regia_core::glam::Vec3;
use regia_render::{LabLocalLight, RenderDevice, TranslucentLayer};
use regia_vfx::format::VfxSystemDef;
use regia_vfx::gpu::{VfxGpu, VfxProfile};
use regia_vfx::lights::VfxFrameLights;
use regia_vfx::medium::VfxMedium;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{Decal, DecalPool, DecalSurface};

use crate::LookdevError;

/// Which DO27 effect the shot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxKind {
    /// The showcase explosion, captured `capture_age` seconds after it fires.
    Fireball,
    /// The same explosion 0.05 s after it fires: the flash.
    Flash,
    /// The showcase smoke column, captured after it has risen.
    Smoke,
    /// Smoke over fire: the smoke column for 4 s, with the explosion going off under it
    /// [`BURN_FIRE_AGE`] before the capture, so its fire lights the smoke from below.
    Burn,
    /// Scripted spawns and decals ([`FxInsert::scripted`]).
    Script,
}

/// One timed spawn of a [`FxKind::Script`] shot (world coordinates).
#[derive(Debug, Clone, PartialEq)]
pub struct FxSpawn {
    /// Library name (the scene plays the medium's variant).
    pub effect: String,
    /// Seconds after the shot starts.
    pub at: f32,
    /// World position at `at`.
    pub position: Vec3,
    /// Instance forward.
    pub forward: Vec3,
    /// The instance moves at this world velocity (a hover hull).
    pub velocity: Vec3,
    /// [`regia_vfx::sim::VfxInstance::set_scale`].
    pub scale: f32,
    /// [`regia_vfx::sim::VfxInstance::set_intensity`].
    pub intensity: f32,
    /// Fires again every this many seconds (a weapon's rate of fire).
    pub repeat: Option<f32>,
    /// Instance seed.
    pub seed: u64,
}

impl FxSpawn {
    /// A one-shot spawn of `effect` at `position` facing `forward`, `at` seconds in.
    #[must_use]
    pub fn new(effect: &str, at: f32, position: Vec3, forward: Vec3) -> Self {
        Self {
            effect: effect.to_string(),
            at,
            position,
            forward,
            velocity: Vec3::ZERO,
            scale: 1.0,
            intensity: 1.0,
            repeat: None,
            seed: 2027,
        }
    }
}

/// A decal laid `at` seconds into a [`FxKind::Script`] shot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FxDecal {
    /// Seconds after the shot starts.
    pub at: f32,
    /// The box.
    pub decal: Decal,
    /// Its material.
    pub surface: DecalSurface,
}

/// The script of a [`FxKind::Script`] shot.
#[derive(Debug, Clone, Default)]
struct Script {
    spawns: Vec<FxSpawn>,
    /// Next fire time per spawn.
    next: Vec<f32>,
    /// Live moving instances: (spawn index, instance id, spawn time).
    moving: Vec<(usize, u64, f32)>,
    decals: Vec<FxDecal>,
    laid: Vec<bool>,
    pool: Option<DecalPool>,
}

/// Age of the explosion in the [`FxKind::Burn`] shot at the capture, seconds: its own smoke has
/// risen over the fireball, which still burns and lights it.
pub const BURN_FIRE_AGE: f32 = 0.8;

/// One effect in a look-dev shot.
pub struct FxInsert {
    gpu: Arc<Mutex<VfxGpu>>,
    dt: Arc<Mutex<f32>>,
    scene: VfxScene,
    kind: FxKind,
    capacity: u32,
    /// Simulated time so far, seconds.
    time: f32,
    /// The explosion of a [`FxKind::Burn`] shot has been spawned.
    fired: bool,
    /// Seconds of simulation per rendered frame.
    pub step_s: f32,
    /// Age of the effect at the captured (last) frame, seconds.
    pub capture_age: f32,
    /// Spherical billboards on (the default) or the flat soft fade (before).
    pub volumetric: bool,
    /// Draw nothing (the shot without its effect, for the effect's contribution).
    pub hidden: bool,
    /// [`FxKind::Burn`]: age of the explosion at the captured frame, seconds
    /// ([`BURN_FIRE_AGE`] by default).
    pub fire_age: f32,
    /// [`FxKind::Script`] state.
    script: Script,
}

impl FxInsert {
    /// The effect, its GPU context (desktop High pool) and the step that captures it at
    /// the right age in `frames` rendered frames.
    ///
    /// # Errors
    ///
    /// The VFX GPU context could not be built.
    pub fn new(device: &RenderDevice, kind: FxKind, frames: u32) -> Result<Self, LookdevError> {
        Self::new_in(device, kind, frames, VfxMedium::earth(), None)
    }

    /// [`Self::new`] in `medium` (the scene plays the medium's explosion and smoke variants,
    /// [`regia_vfx::showcase::explosion_in`]), captured at `age` seconds instead of the
    /// kind's default.
    ///
    /// # Errors
    ///
    /// The VFX GPU context could not be built.
    pub fn new_in(
        device: &RenderDevice,
        kind: FxKind,
        frames: u32,
        medium: VfxMedium,
        age: Option<f32>,
    ) -> Result<Self, LookdevError> {
        let mut profile = VfxProfile::desktop_high();
        // FX_SOFT_RES=full|half|quarter: the soft-particle layer (evidence before/after).
        if let Some(res) = std::env::var("FX_SOFT_RES")
            .ok()
            .and_then(|v| regia_vfx::gpu::SoftParticleRes::from_name(&v))
        {
            profile.soft_particle_res = res;
        }
        let gpu = VfxGpu::new(device, profile).map_err(|e| LookdevError::Config(format!("vfx: {e}")))?;
        let mut library = VfxLibrary::new();
        let mut defs = match kind {
            FxKind::Fireball | FxKind::Flash => vec![regia_vfx::showcase::explosion()],
            FxKind::Smoke => vec![regia_vfx::showcase::smoke()],
            FxKind::Burn => vec![regia_vfx::showcase::smoke(), regia_vfx::showcase::explosion()],
            FxKind::Script => Vec::new(),
        };
        defs.extend(regia_vfx::showcase::variants_in(&medium));
        // Look-dev knob: FX_EMISSIVE_SCALE=<k> scales every emissive emitter (tuning only).
        let scale = std::env::var("FX_EMISSIVE_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok());
        for mut def in defs {
            if let Some(k) = scale {
                for emitter in &mut def.emitters {
                    if emitter.render.emissive > 1.0 {
                        emitter.render.emissive *= k;
                    }
                }
            }
            library
                .add(def)
                .map_err(|e| LookdevError::Config(format!("vfx effect: {e}")))?;
        }
        let (default_age, frames) = match kind {
            // The fireball body at its fullest; smoke after the column has risen.
            FxKind::Fireball => (0.45, frames.max(2)),
            FxKind::Flash => (0.05, frames.max(2)),
            FxKind::Smoke => (6.0, frames.max(2)),
            // Render it with at least 48 frames (`fx_shots` does), so the explosion inside
            // the 4 s run steps at 1/12 s.
            FxKind::Burn => (4.0, frames.max(2)),
            FxKind::Script => (1.0, frames.max(2)),
        };
        let capture_age = age.filter(|a| *a > 0.0).unwrap_or(default_age);
        let mut scene = VfxScene::new(library);
        scene.set_medium(medium);
        Ok(Self {
            gpu: Arc::new(Mutex::new(gpu)),
            dt: Arc::new(Mutex::new(capture_age / frames as f32)),
            scene,
            kind,
            capacity: profile.particle_capacity,
            time: 0.0,
            fired: false,
            step_s: capture_age / frames as f32,
            capture_age,
            volumetric: true,
            hidden: false,
            fire_age: BURN_FIRE_AGE,
            script: Script::default(),
        })
    }

    /// A scripted shot: `defs` (and their `medium` variants when given) in the library, the
    /// `spawns` and `decals` played on their times, captured at `capture_age` seconds over
    /// `frames` rendered frames.
    ///
    /// # Errors
    ///
    /// The VFX GPU context could not be built, or a definition is invalid.
    pub fn scripted(
        device: &RenderDevice,
        frames: u32,
        medium: VfxMedium,
        capture_age: f32,
        defs: Vec<VfxSystemDef>,
        spawns: Vec<FxSpawn>,
        decals: Vec<FxDecal>,
    ) -> Result<Self, LookdevError> {
        let mut fx = Self::new_in(device, FxKind::Script, frames, medium, Some(capture_age))?;
        for def in defs {
            fx.scene
                .library_mut()
                .add(def)
                .map_err(|e| LookdevError::Config(format!("vfx effect: {e}")))?;
        }
        fx.script = Script {
            next: spawns.iter().map(|s| s.at).collect(),
            spawns,
            moving: Vec::new(),
            laid: vec![false; decals.len()],
            decals,
            pool: Some(DecalPool::new(512)),
        };
        Ok(fx)
    }

    /// The scene (the library holds every effect the shot can play).
    pub fn scene_mut(&mut self) -> &mut VfxScene {
        &mut self.scene
    }

    /// Plays the script's events that fall in `[t, t + step)`, moves the moving instances and
    /// ages the decals.
    fn run_script(&mut self, t: f32) {
        let step = self.step_s;
        let script = &mut self.script;
        for (i, spawn) in script.spawns.iter().enumerate() {
            let mut k = 0u64;
            while script.next[i] < t + step - 1e-6 && k < 64 {
                let fire_t = script.next[i];
                let position = spawn.position + spawn.velocity * (t - spawn.at).max(0.0);
                let seed = spawn.seed.wrapping_add((fire_t * 1000.0) as u64);
                if let Some(id) = self.scene.spawn(&spawn.effect, position, spawn.forward, seed) {
                    if let Some(instance) = self.scene.instance_mut(id) {
                        instance.set_scale(spawn.scale);
                        instance.set_intensity(spawn.intensity);
                    }
                    if spawn.velocity != Vec3::ZERO {
                        script.moving.push((i, id, spawn.at));
                    }
                }
                script.next[i] = match spawn.repeat {
                    Some(period) if period > 1e-3 => fire_t + period,
                    _ => f32::INFINITY,
                };
                k += 1;
            }
        }
        for &(i, id, at) in &script.moving {
            let spawn = &script.spawns[i];
            let position = spawn.position + spawn.velocity * (t + step - at).max(0.0);
            if let Some(instance) = self.scene.instance_mut(id) {
                instance.set_transform(position, spawn.forward, step);
            }
        }
        if let Some(pool) = script.pool.as_mut() {
            for (d, laid) in script.decals.iter().zip(script.laid.iter_mut()) {
                if !*laid && d.at < t + step - 1e-6 {
                    let mut decal = d.decal;
                    decal.age = (t + step - d.at).max(0.0) - step;
                    pool.spawn_with(decal, d.surface);
                    *laid = true;
                }
            }
            pool.update(step);
        }
    }

    /// The particle lighting of the last rendered frame (sun, ambient, exposure).
    #[must_use]
    pub fn lighting(&self) -> Option<regia_vfx::gpu::VfxLighting> {
        self.gpu.lock().ok().map(|gpu| gpu.lighting())
    }

    /// The translucent layer the frame renderer draws the effect with.
    #[must_use]
    pub fn layer(&self) -> Arc<dyn TranslucentLayer> {
        regia_vfx::translucent_layer_with_dt(Arc::clone(&self.gpu), Arc::clone(&self.dt))
    }

    /// Advances one rendered frame (`index` from 0) with the effect at `origin` on the
    /// ground; returns the effect's particle lights for the frame.
    ///
    /// Effects spawn with forward +Z, the showcase convention: local +Y (the effects' "up") is
    /// world up. A host that passes the ground normal as forward turns local +Y sideways.
    pub fn step(&mut self, index: u32, origin: Vec3, eye: Vec3) -> Vec<LabLocalLight> {
        if index == 0 && !self.hidden {
            let name = match self.kind {
                FxKind::Fireball | FxKind::Flash => Some("explosion"),
                FxKind::Smoke | FxKind::Burn => Some("smoke"),
                FxKind::Script => None,
            };
            if let Some(name) = name {
                let _ = self.scene.spawn(name, origin, Vec3::Z, 2027);
            }
        }
        if self.kind == FxKind::Script && !self.hidden {
            self.run_script(self.time);
        }
        // Smoke over fire: the explosion goes off under the column so it is `fire_age` old at
        // the captured frame, to the nearest step.
        if self.kind == FxKind::Burn
            && !self.hidden
            && !self.fired
            && self.time >= self.capture_age - self.fire_age - 0.5 * self.step_s
        {
            let _ = self.scene.spawn("explosion", origin, Vec3::Z, 2028);
            self.fired = true;
        }
        self.time += self.step_s;
        if let Ok(mut dt) = self.dt.lock() {
            *dt = self.step_s;
        }
        self.scene
            .update(self.step_s, &LodInput { camera_pos: eye }, self.capacity);
        let mut lights = VfxFrameLights::default();
        self.scene.gather_lights(&mut lights, 4);
        if let Ok(mut gpu) = self.gpu.lock() {
            gpu.set_volumetric_billboards(self.volumetric);
            if let Some(pool) = self.script.pool.as_ref() {
                gpu.write_decals_with(pool.decals(), pool.surfaces());
            }
            self.scene.upload(&mut gpu);
            gpu.set_fire_light(lights.lights.first().map(|l| (l.position, l.color, l.radius)));
        }
        regia_vfx::particle_point_lights(&lights)
    }
}

/// How hard the effect meets the scene: from the shot rendered with and without the effect,
/// the effect's contribution `c` (display-linear luma difference) and the pixels where it
/// drops by more than 4× to a neighbour while being visible (`c > 0.12`): a cut, not a fade.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HardEdges {
    /// Pixels with a hard cut.
    pub pixels: u32,
    /// Longest near-horizontal run of cut pixels (a row ±1), in pixels: a sprite plane
    /// meeting the ground draws a long straight one.
    pub longest_run: u32,
    /// Pixels the effect covers (`c > 0.02`).
    pub covered: u32,
}

fn luma_lin(px: &[u8; 4]) -> f32 {
    let lin = |b: u8| {
        let c = f32::from(b) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(px[0]) + 0.7152 * lin(px[1]) + 0.0722 * lin(px[2])
}

/// Measures [`HardEdges`] of an effect frame against the same frame without the effect.
#[must_use]
pub fn hard_edges(with: &[u8], without: &[u8], width: u32, height: u32) -> HardEdges {
    let (w, h) = (width as usize, height as usize);
    let c: Vec<f32> = with
        .as_chunks::<4>()
        .0
        .iter()
        .zip(without.as_chunks::<4>().0)
        .map(|(a, b)| (luma_lin(a) - luma_lin(b)).abs())
        .collect();
    let mut cut = vec![false; w * h];
    let mut pixels = 0u32;
    let mut covered = 0u32;
    for y in 0..h {
        for x in 0..w {
            let v = c[y * w + x];
            if v > 0.02 {
                covered += 1;
            }
            if v <= 0.12 {
                continue;
            }
            let neighbours = [
                (x > 0).then(|| c[y * w + x - 1]),
                (x + 1 < w).then(|| c[y * w + x + 1]),
                (y > 0).then(|| c[(y - 1) * w + x]),
                (y + 1 < h).then(|| c[(y + 1) * w + x]),
            ];
            if neighbours.into_iter().flatten().any(|n| n < 0.25 * v) {
                cut[y * w + x] = true;
                pixels += 1;
            }
        }
    }
    let mut longest_run = 0u32;
    for y in 0..h {
        let mut run = 0u32;
        for x in 0..w {
            let hit = (y.saturating_sub(1)..=(y + 1).min(h - 1)).any(|yy| cut[yy * w + x]);
            if hit {
                run += 1;
                longest_run = longest_run.max(run);
            } else {
                run = 0;
            }
        }
    }
    HardEdges {
        pixels,
        longest_run,
        covered,
    }
}

/// How an effect reads against its background ([`daylight_read`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DaylightRead {
    /// Pixels the effect changes (a display channel moves by more than 6/255).
    pub covered: u32,
    /// Of those, pixels clipped to white (every channel at 250 or more) that were not already.
    pub clipped: u32,
    /// Mean change in display chroma (`rgb − mean(rgb)`, length, 0..1) over the covered pixels:
    /// how much colour the effect puts on the background.
    pub chroma_contrast: f32,
    /// Mean change in display luma over the covered pixels.
    pub luma_contrast: f32,
    /// Mean display saturation (`(max − min) / max`) of the covered pixels with the effect.
    pub saturation: f32,
}

/// How readable an effect is: from the shot with and without it, the pixels it covers, how
/// many it clips to white, and how much colour and brightness it adds there. On bright ice an
/// additive effect adds brightness that clips; an effect drawn over the scene adds colour.
#[must_use]
pub fn daylight_read(with: &[u8], without: &[u8], width: u32, height: u32) -> DaylightRead {
    let n = (width * height) as usize;
    let mut covered = 0u32;
    let mut clipped = 0u32;
    let (mut chroma, mut luma, mut sat) = (0.0f64, 0.0f64, 0.0f64);
    for (a, b) in with
        .as_chunks::<4>()
        .0
        .iter()
        .zip(without.as_chunks::<4>().0)
        .take(n)
    {
        let moved = (0..3).any(|c| a[c].abs_diff(b[c]) > 6);
        if !moved {
            continue;
        }
        covered += 1;
        let white = |p: &[u8; 4]| p[0] >= 250 && p[1] >= 250 && p[2] >= 250;
        if white(a) && !white(b) {
            clipped += 1;
        }
        let f = |p: &[u8; 4]| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        };
        let (pa, pb) = (f(a), f(b));
        let mean = |p: [f32; 3]| (p[0] + p[1] + p[2]) / 3.0;
        let (ma, mb) = (mean(pa), mean(pb));
        let dc: f32 = (0..3)
            .map(|c| ((pa[c] - ma) - (pb[c] - mb)).powi(2))
            .sum::<f32>()
            .sqrt();
        chroma += f64::from(dc);
        let l = |p: [f32; 3]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
        luma += f64::from((l(pa) - l(pb)).abs());
        let maxc = pa[0].max(pa[1]).max(pa[2]);
        let minc = pa[0].min(pa[1]).min(pa[2]);
        sat += f64::from(if maxc > 1e-4 { (maxc - minc) / maxc } else { 0.0 });
    }
    let k = f64::from(covered.max(1));
    DaylightRead {
        covered,
        clipped,
        chroma_contrast: (chroma / k) as f32,
        luma_contrast: (luma / k) as f32,
        saturation: (sat / k) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_over_white_reads_and_added_light_clips() {
        let (w, h) = (8u32, 8u32);
        let ice = vec![240u8; (w * h * 4) as usize];
        // An orange core drawn over the ice, and the same light added onto it (clips white).
        let mut over = ice.clone();
        let mut added = ice.clone();
        for i in 0..16 {
            over[i * 4..i * 4 + 3].copy_from_slice(&[255, 150, 40]);
            added[i * 4..i * 4 + 3].copy_from_slice(&[255, 255, 252]);
        }
        let o = daylight_read(&over, &ice, w, h);
        let a = daylight_read(&added, &ice, w, h);
        assert_eq!(o.covered, 16);
        assert_eq!(o.clipped, 0);
        assert_eq!(a.clipped, 16);
        assert!(o.chroma_contrast > 0.3 && a.chroma_contrast < 0.05, "{o:?} {a:?}");
        assert!(o.saturation > 0.7);
    }

    #[test]
    fn a_straight_cut_is_long_and_a_fade_is_not() {
        let (w, h) = (64u32, 32u32);
        let without = vec![40u8; (w * h * 4) as usize];
        // A bright card cut flat at row 20 (a sprite plane in the ground).
        let mut cut = without.clone();
        // The same card fading out over ten rows.
        let mut faded = without.clone();
        for y in 0..h {
            for x in 8..56 {
                let i = ((y * w + x) * 4) as usize;
                if y < 20 {
                    cut[i..i + 3].copy_from_slice(&[230, 230, 230]);
                }
                let k = if y < 12 {
                    1.0
                } else {
                    (1.0 - (y as f32 - 12.0) / 10.0).max(0.0)
                };
                let v = (40.0 + 190.0 * k) as u8;
                faded[i..i + 3].copy_from_slice(&[v, v, v]);
            }
        }
        let hard = hard_edges(&cut, &without, w, h);
        let soft = hard_edges(&faded, &without, w, h);
        assert!(hard.longest_run >= 48, "{hard:?}");
        assert!(soft.longest_run < 8, "{soft:?}");
    }
}
