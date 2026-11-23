// SPDX-License-Identifier: MIT-0
//! Headless crowd lab (`docs/specs/85-retarget-crowds.md` § 6, acceptance).
//!
//! One procedural locomotion library (idle, walk, run with planted feet) is authored on a plain
//! rig and retargeted onto three rigs in real-world exporter conventions: a centimetre
//! Mixamo-named rig, a game-named A-pose rig facing +Z with rolled bind frames and twist/IK
//! joints, and a short Biped-named rig. Every retarget is checked with
//! `Retargeter::check`. Then `agents` navigation agents (`regia_nav::NavCrowd`) cross a plaza
//! in two opposing flows at mixed speeds; each fixed step feeds their positions and velocities
//! to a `CrowdAnimator` (LOD tiers from a viewer at the plaza's south edge, pose sharing, every
//! fifth agent with its own proportions). The run reports the crowd counters, the planted-foot
//! skate of exact (tier 0) agents and a digest of the whole output: equal inputs give equal
//! digests, whatever the chunk size. [`ClipShape`] runs the same lab on the clip shapes an
//! importer hands over when its root-motion setting is off.

use regia_animation::demo_rig::{
    DEMO_LEGS, DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
    demo_travelling_hips,
};
use regia_animation::retarget::Retargeter;
use regia_animation::{
    AnimationClip, CrowdAgentDesc, CrowdAnimator, CrowdArchetype, CrowdSettings, InstanceBindPose,
};
use regia_core::math::Vec3;
use regia_nav::{CrowdAgentParams, NavCrowd, NavCrowdConfig, NavMeshBuildParams, NavMeshBuilder};

/// Where the lab's clips carry their ground travel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipShape {
    /// A root-motion track on the root joint.
    #[default]
    RootMotion,
    /// No root-motion track, a still root: a treadmill clip.
    InPlace,
    /// No root-motion track, the travel on the hips.
    TravellingHips,
}

/// What to run.
#[derive(Clone, Debug, PartialEq)]
pub struct LabConfig {
    /// Navigation agents (and crowd agents).
    pub agents: usize,
    /// Fixed steps (30 Hz).
    pub steps: usize,
    /// Crowd chunk size (0 = serial).
    pub chunk: usize,
    /// How the clips carry their travel.
    pub shape: ClipShape,
}

impl Default for LabConfig {
    fn default() -> Self {
        LabConfig {
            agents: 240,
            steps: 300,
            chunk: 32,
            shape: ClipShape::default(),
        }
    }
}

/// One retargeted rig.
#[derive(Clone, Debug, PartialEq)]
pub struct RigSummary {
    /// Archetype name.
    pub name: String,
    /// Translation scale.
    pub scale: f32,
    /// Facing yaw, degrees.
    pub facing_yaw_deg: f32,
    /// Worst bone-direction error over the three clips, degrees.
    pub max_direction_error_deg: f32,
    /// Issue codes other than `facing_turned`, as `clip:code`.
    pub issues: Vec<String>,
}

/// What a run produced.
#[derive(Clone, Debug, PartialEq)]
pub struct LabSummary {
    /// The rigs.
    pub rigs: Vec<RigSummary>,
    /// Agents.
    pub agents: usize,
    /// Steps run.
    pub steps: usize,
    /// Poses evaluated over the run.
    pub evaluated_poses: usize,
    /// Due agents that reused another agent's pose, over the run.
    pub shared_hits: usize,
    /// Agent updates over the run.
    pub updated: usize,
    /// Agent updates skipped by LOD intervals over the run.
    pub skipped: usize,
    /// Mean planted-foot skate of exact (tier 0) agents, m/s: the world speed of feet the gait
    /// plants in two consecutive steps (`CrowdAgentState::feet_planted`).
    pub mean_skate: f32,
    /// Mean ground speed of those agents over the same frames, m/s.
    pub mean_speed: f32,
    /// Planted foot-steps measured.
    pub skate_frames: usize,
    /// Crowd animation digest.
    pub digest: u64,
    /// Navigation digest.
    pub nav_digest: u64,
}

impl LabSummary {
    /// The summary as printed lines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for rig in &self.rigs {
            out.push(format!(
                "crowd_lab: rig {} scale={:.3} facing={:.0}deg max_dir_err={:.4}deg issues={:?}",
                rig.name, rig.scale, rig.facing_yaw_deg, rig.max_direction_error_deg, rig.issues
            ));
        }
        out.push(format!(
            "crowd_lab: agents={} steps={} evaluated={} shared={} updated={} skipped={}",
            self.agents, self.steps, self.evaluated_poses, self.shared_hits, self.updated, self.skipped
        ));
        out.push(format!(
            "crowd_lab: skate={:.4} m/s over {} planted frames (mean speed {:.3} m/s)",
            self.mean_skate, self.skate_frames, self.mean_speed
        ));
        out.push(format!(
            "crowd_lab: digest={:016x} nav_digest={:016x}",
            self.digest, self.nav_digest
        ));
        out
    }
}

/// Two triangles covering `[x0, x1] × [z0, z1]` at height 0.
fn rect(x0: f32, z0: f32, x1: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

/// A small deterministic hash for per-agent variety.
fn hash(i: usize, salt: u32) -> f32 {
    let mut x = (i as u32).wrapping_mul(0x9e37_79b9) ^ salt.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    (x & 0xffff) as f32 / 65535.0
}

/// Retarget the library onto the three rigs and build their archetypes.
fn archetypes(shape: ClipShape) -> Result<(Vec<CrowdArchetype>, Vec<RigSummary>), String> {
    let source = demo_humanoid(&DemoRig::default()).ok_or("source rig")?;
    let clips = demo_locomotion(&source, &DemoGait::standard());
    let clips: Vec<AnimationClip> = match shape {
        ClipShape::RootMotion => clips,
        ClipShape::InPlace => clips.iter().map(demo_in_place).collect(),
        ClipShape::TravellingHips => clips.iter().map(demo_travelling_hips).collect(),
    };
    let mut archetypes = Vec::new();
    let mut summaries = Vec::new();
    for (name, rig) in lab_rigs() {
        let target = demo_humanoid(&rig).ok_or_else(|| format!("{name}: rig"))?;
        let rt = Retargeter::humanoid(&source, &target).map_err(|e| format!("{name}: {e}"))?;
        let mut worst = 0.0f32;
        let mut issues = Vec::new();
        for clip in &clips {
            let report = rt.check(Some(clip));
            if let Some(check) = &report.clip {
                worst = worst.max(check.max_direction_error_deg);
            }
            issues.extend(
                report
                    .issues
                    .iter()
                    .filter(|i| i.code != "facing_turned")
                    .map(|i| format!("{}:{}", clip.name, i.code)),
            );
        }
        summaries.push(RigSummary {
            name: name.to_string(),
            scale: rt.scale(),
            facing_yaw_deg: rt.facing_yaw().to_degrees(),
            max_direction_error_deg: worst,
            issues,
        });
        archetypes.push(CrowdArchetype::retargeted(name, &rt, &clips).map_err(|e| format!("{name}: {e}"))?);
    }
    Ok((archetypes, summaries))
}

/// One agent walks `archetype` at the speed of its fastest gait for 120 steps; returns the mean
/// world speed of feet the gait plants in two consecutive steps, m/s, and the frames measured.
fn walk_skate(archetype: &CrowdArchetype, foot_lock: bool) -> Result<(f32, usize), String> {
    let step = 1.0 / 30.0;
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        fixed_step: step,
        chunk: 0,
        foot_lock,
        lods: vec![regia_animation::CrowdLod {
            distance: f32::INFINITY,
            interval: 1,
            phase_buckets: 0,
        }],
        ..CrowdSettings::default()
    });
    let kind = crowd.add_archetype(archetype.clone());
    let id = crowd
        .add_agent(CrowdAgentDesc {
            archetype: kind,
            ..CrowdAgentDesc::default()
        })
        .map_err(|e| e.to_string())?;
    let speed = archetype.gaits().last().map_or(0.0, |g| g.speed);
    let velocity = Vec3::NEG_Z * speed;
    let mut last: Option<([Vec3; 2], [bool; 2])> = None;
    let (mut position, mut sum, mut frames) = (Vec3::ZERO, 0.0f32, 0usize);
    for _ in 0..120 {
        position += velocity * step;
        crowd.set_motion(id, position, velocity);
        crowd.step();
        let feet = [0, 1].map(|s| crowd.joint_world_position(id, DEMO_LEGS[s][2]));
        let (Some(state), [Some(l), Some(r)]) = (crowd.agent_state(id), feet) else {
            return Err("an agent was not evaluated".into());
        };
        if let Some((before, was)) = last {
            for side in 0..2 {
                if was[side] && state.feet_planted[side] {
                    let d = [l, r][side] - before[side];
                    sum += Vec3::new(d.x, 0.0, d.z).length() / step;
                    frames += 1;
                }
            }
        }
        last = Some(([l, r], state.feet_planted));
    }
    Ok((sum / frames.max(1) as f32, frames))
}

/// The real-content check (`crowd_lab --cesium`, spec 85 § 6): load the Khronos CesiumMan
/// skeleton and clips from `anim_lab`'s cooked cache (`target/anim_cache/project`, made by
/// running `anim_lab` once; CesiumMan is CC BY 4.0), guess its humanoid roles, retarget every
/// clip onto the three lab rigs and return one printed line per rig and clip, then one per rig
/// for the clips played as a crowd gait (measured speed, foot skate with and without locking).
///
/// # Errors
///
/// A missing or unreadable cache (the message says how to make it), or a retarget that cannot
/// be built.
pub fn cesium_report(project: &std::path::Path) -> Result<Vec<String>, String> {
    use regia_animation::cooked::{load_cooked_clip, load_cooked_skeleton};
    let cache = project.join(".regia").join("cooked").join(std::env::consts::OS);
    let manifest_path = cache.join("asset_manifest.bin");
    let bytes = std::fs::read(&manifest_path).map_err(|e| {
        format!(
            "read {}: {e} (run `cargo run -p anim_lab` once to download and cook CesiumMan)",
            manifest_path.display()
        )
    })?;
    let manifest = regia_asset::AssetManifest::from_postcard(&bytes)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let mut source = None;
    let mut clips = Vec::new();
    for entry in &manifest.entries {
        if !entry.path.contains("CesiumMan") {
            continue;
        }
        let file = cache.join(format!("{}.fckd", entry.id));
        if entry.type_path == "regia_animation::Skeleton" {
            let bytes = std::fs::read(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
            let (_, skeleton) =
                load_cooked_skeleton(&bytes).map_err(|e| format!("skeleton {}: {e}", entry.id))?;
            source = Some(skeleton);
        } else if entry.type_path == "regia_animation::AnimationClip" {
            let bytes = std::fs::read(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
            let (_, clip) = load_cooked_clip(&bytes).map_err(|e| format!("clip {}: {e}", entry.id))?;
            clips.push(clip);
        }
    }
    let source = source.ok_or("no CesiumMan skeleton in the cooked cache")?;
    if clips.is_empty() {
        return Err("no CesiumMan clip in the cooked cache".into());
    }
    let roles = regia_animation::retarget::RetargetProfile::guess_humanoid(&source);
    let mut out = vec![format!(
        "crowd_lab: CesiumMan joints={} roles={} clips={}",
        source.len(),
        roles.roles.len(),
        clips.len()
    )];
    for (name, rig) in lab_rigs() {
        let target = demo_humanoid(&rig).ok_or_else(|| format!("{name}: rig"))?;
        let rt = Retargeter::humanoid(&source, &target).map_err(|e| format!("CesiumMan -> {name}: {e}"))?;
        for clip in &clips {
            let report = rt.check(Some(clip));
            let issues: Vec<&str> = report.issues.iter().map(|i| i.code).collect();
            let (worst, mean) = report.clip.as_ref().map_or((0.0, 0.0), |c| {
                (c.max_direction_error_deg, c.mean_direction_error_deg)
            });
            let check = report.clip.as_ref();
            let stance = check.map(|c| (c.source_stance_speed, c.target_stance_speed));
            let skate = check.map(|c| (c.source_foot_skate, c.target_foot_skate));
            out.push(format!(
                "crowd_lab: CesiumMan {} -> {name}: mapped={} missing={:?} scale={:.3} legs={:?} facing={:.0}deg \
                 dir_err max={worst:.2}deg mean={mean:.2}deg stance={stance:?} skate={skate:?} \
                 issues={issues:?}",
                clip.name,
                report.mapped.len(),
                report.missing_humanoid_roles,
                report.scale,
                report.leg_ratio,
                report.facing_yaw_deg,
            ));
        }
        let archetype = CrowdArchetype::retargeted(name, &rt, &clips).map_err(|e| format!("{name}: {e}"))?;
        let gaits: Vec<String> = archetype
            .gaits()
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let planted = archetype.planted_share(i);
                format!(
                    "{}={:.3}m/s/{:.3}s planted={planted:?}",
                    g.clip.name, g.speed, g.cycle
                )
            })
            .collect();
        let (locked, frames) = walk_skate(&archetype, true)?;
        let (free, _) = walk_skate(&archetype, false)?;
        out.push(format!(
            "crowd_lab: CesiumMan crowd on {name}: gaits={gaits:?} forward={:.2} legs={} \
             skate locked={locked:.4} m/s unlocked={free:.4} m/s over {frames} planted frames",
            archetype.forward(),
            archetype.has_legs(),
        ));
    }
    Ok(out)
}

/// FNV-1a over every agent's simulation state (phase, rate, weight, speed, yaw, position, hip
/// ratio, gait, tier and foot flags), in agent order: the crowd digest without the skinning
/// palette, so a trace can tell a gameplay-state difference from a pose-matrix one.
fn motion_digest(
    crowd: &CrowdAnimator,
    pairs: &[(regia_nav::CrowdAgentId, regia_animation::CrowdAgentId)],
) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut eat = |v: u32| {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for &(_, id) in pairs {
        let Some(s) = crowd.agent_state(id) else {
            eat(u32::MAX);
            continue;
        };
        for v in [
            s.phase,
            s.rate,
            s.weight,
            s.speed,
            s.yaw,
            s.position.x,
            s.position.y,
            s.position.z,
            s.hip_ratio,
        ] {
            eat(v.to_bits());
        }
        let flags = u32::from(s.feet_locked[0])
            | u32::from(s.feet_locked[1]) << 1
            | u32::from(s.feet_planted[0]) << 2
            | u32::from(s.feet_planted[1]) << 3;
        for v in [s.gait, s.tier] {
            eat(u32::try_from(v).unwrap_or(u32::MAX));
        }
        eat(flags);
    }
    h
}

/// The three lab rigs (module docs).
fn lab_rigs() -> [(&'static str, DemoRig); 3] {
    [
        (
            "mixamo-cm",
            DemoRig {
                naming: RigNaming::Mixamo,
                centimetres: true,
                ..DemoRig::default()
            },
        ),
        (
            "game-apose-plusz",
            DemoRig {
                naming: RigNaming::Game,
                facing_positive_z: true,
                rolled_frames: true,
                a_pose: true,
                extra_joints: true,
                size: 0.95,
                ..DemoRig::default()
            },
        ),
        (
            "biped-short",
            DemoRig {
                naming: RigNaming::Biped,
                size: 0.8,
                ..DemoRig::default()
            },
        ),
    ]
}

/// Run the lab.
///
/// # Errors
///
/// A rig, retarget, navigation mesh or agent that fails to build (a bug in the lab).
pub fn run(config: &LabConfig) -> Result<LabSummary, String> {
    run_traced(config, &mut |_, _, _| {})
}

/// [`run`], reporting the navigation digest (section `nav`), the crowd animation digest (section
/// `crowd`) and the crowd's simulation state without the skinning palette (section `crowd_motion`)
/// after every step to `trace` as `(section, step, digest)`. Two hosts whose
/// summaries differ name their first diverging step and section by the first differing line.
///
/// # Errors
///
/// As [`run`].
pub fn run_traced(config: &LabConfig, trace: &mut dyn FnMut(&str, u32, u64)) -> Result<LabSummary, String> {
    let (archetypes, rigs) = archetypes(config.shape)?;
    let step = 1.0 / 30.0;
    // A 40 m × 40 m plaza.
    let half = 20.0;
    let tris: Vec<[Vec3; 3]> = rect(-half, -half, half, half).to_vec();
    let mesh = NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .map_err(|e| format!("navmesh: {e}"))?;
    let mut nav = NavCrowd::new(NavCrowdConfig {
        step_seconds: step,
        max_agents: config.agents.max(1),
        ..NavCrowdConfig::default()
    })
    .map_err(|e| format!("nav crowd: {e}"))?;
    let mut crowd = CrowdAnimator::new(CrowdSettings {
        fixed_step: step,
        chunk: config.chunk,
        ..CrowdSettings::default()
    });
    let kinds: Vec<usize> = archetypes.into_iter().map(|a| crowd.add_archetype(a)).collect();
    let mut pairs = Vec::with_capacity(config.agents);
    for i in 0..config.agents {
        // Two flows: west → east and east → west, in rows.
        let west = i % 2 == 0;
        let row = (i / 2) as f32;
        let z = -16.0 + (row * 1.3) % 32.0;
        let x = if west { -17.0 } else { 17.0 } + (hash(i, 1) - 0.5) * 3.0;
        let start = Vec3::new(x, 0.0, z);
        let goal = Vec3::new(-x, 0.0, z + (hash(i, 2) - 0.5) * 6.0);
        let speed = 1.0 + 2.0 * hash(i, 3);
        let nav_id = nav
            .add_agent(&mesh, start, CrowdAgentParams::new(0.3, 1.7, speed, 6.0))
            .map_err(|e| format!("agent {i}: {e}"))?;
        nav.set_target(&mesh, nav_id, goal)
            .map_err(|e| format!("agent {i} target: {e}"))?;
        let archetype = kinds[i % kinds.len()];
        let proportions = if i.is_multiple_of(5) {
            let skeleton = &crowd.archetypes()[archetype].skeleton;
            Some(
                InstanceBindPose::uniform(skeleton, 0.9 + 0.2 * hash(i, 4))
                    .map_err(|e| format!("agent {i}: {e}"))?,
            )
        } else {
            None
        };
        let id = crowd
            .add_agent(CrowdAgentDesc {
                archetype,
                position: start,
                phase: hash(i, 5),
                proportions,
                ..CrowdAgentDesc::default()
            })
            .map_err(|e| format!("agent {i}: {e}"))?;
        pairs.push((nav_id, id));
    }
    crowd.set_viewer(Some(Vec3::new(0.0, 1.7, -25.0)));

    let mut totals = (0usize, 0usize, 0usize, 0usize);
    // Planted-foot tracking: last world positions of both feet and which ones the gait planted
    // (exact tiers only; `feet_planted` comes from the clips' contact tables).
    let mut last: Vec<Option<([Vec3; 2], [bool; 2])>> = vec![None; pairs.len()];
    let (mut skate, mut speed_sum, mut frames) = (0.0f64, 0.0f64, 0usize);
    for n in 0..config.steps {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        nav.step(&mesh);
        trace("nav", n, nav.digest());
        for &(nav_id, id) in &pairs {
            if let Some(agent) = nav.agent(nav_id) {
                crowd.set_motion(id, agent.position(), agent.velocity());
            }
        }
        crowd.step();
        trace("crowd", n, crowd.digest());
        trace("crowd_motion", n, motion_digest(&crowd, &pairs));
        let stats = crowd.stats();
        totals.0 += stats.evaluated_poses;
        totals.1 += stats.shared_hits;
        totals.2 += stats.updated;
        totals.3 += stats.skipped;
        for (k, &(_, id)) in pairs.iter().enumerate() {
            let Some(state) = crowd.agent_state(id) else {
                continue;
            };
            let feet = [0, 1].map(|s| crowd.joint_world_position(id, DEMO_LEGS[s][2]));
            let [Some(l), Some(r)] = feet else {
                last[k] = None;
                continue;
            };
            let now = [l, r];
            if state.tier == 0
                && state.speed > 0.3
                && let Some((before, was)) = last[k]
            {
                for side in 0..2 {
                    if state.feet_planted[side] && was[side] {
                        let d = now[side] - before[side];
                        skate += f64::from(Vec3::new(d.x, 0.0, d.z).length() / step);
                        speed_sum += f64::from(state.speed);
                        frames += 1;
                    }
                }
            }
            last[k] = (state.tier == 0).then_some((now, state.feet_planted));
        }
    }
    let mean = |sum: f64| {
        if frames == 0 {
            0.0
        } else {
            (sum / frames as f64) as f32
        }
    };
    Ok(LabSummary {
        rigs,
        agents: config.agents,
        steps: config.steps,
        evaluated_poses: totals.0,
        shared_hits: totals.1,
        updated: totals.2,
        skipped: totals.3,
        mean_skate: mean(skate),
        mean_speed: mean(speed_sum),
        skate_frames: frames,
        digest: crowd.digest(),
        nav_digest: nav.digest(),
    })
}
