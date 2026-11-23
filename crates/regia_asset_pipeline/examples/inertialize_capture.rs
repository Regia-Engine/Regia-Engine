// SPDX-License-Identifier: MPL-2.0
//! Inertialized transitions on DO27's pilot rig: before/after captures and a CPU cost
//! comparison against cross-fades (ADR-203, `book/user-guide/inertialization.md`).
//!
//! `REGIA_DO27_CONTENT=<do27-content> cargo run --release -p regia_asset_pipeline
//!  --example inertialize_capture -- <out-dir> [characters]`
//!
//! Loads `pilots/anims/pilot.glb` (skeleton + clips) and `pilots/anims/body.animgraph.ron`
//! read-only, then:
//!
//! 1. **Captures** three switches with the graph as authored (cross-fades, snapped layer
//!    weight) and with every transition set to `Inertialize` at max(duration, 0.15 s) (and the aim layer's
//!    `weight_inertialize = 0.15`): walk → aim (FireWeight 0 → 1), run → air → land, and
//!    run → death (DO27's 0.05 s cross-fade). For each it writes `<name>.csv` (per frame: a
//!    joint's angle from bind and its angular speed, both modes) and `<name>.png`: top panel
//!    the joint's angle from bind (degrees), bottom panel its angular speed (deg/s); red =
//!    cross-fade, blue = inertialized, grey verticals = parameter changes. `<name>_strip.png` is
//!    a side-view stick-figure frame strip across the switch (top row cross-fade, bottom row
//!    inertialized, every second frame at 60 Hz).
//! 2. **Cost**: `characters` (default 256) pilots, each with its own instance and staggered
//!    parameter changes that keep transitions running, evaluated for 600 frames at 60 Hz per
//!    mode (best of 3). Prints and writes `cost.txt`: microseconds per character per frame
//!    overall, for a character mid-transition, and otherwise (each character timed alone).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use regia_animation::eval::evaluate_graph;
use regia_animation::graph::AnimNode;
use regia_animation::{
    AnimGraph, ClipLibrary, EvalContext, GraphInstance, ParameterSet, Pose, Skeleton, TransitionBlend,
};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;

const DT: f32 = 1.0 / 60.0;

fn main() {
    if let Err(e) = run() {
        eprintln!("inertialize_capture: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| "inertialize-out".into()));
    let characters: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(256);
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let content = regia_core::brand::env_var("REGIA_DO27_CONTENT")
        .map_err(|_| "REGIA_DO27_CONTENT is not set (DO27's read-only content drop)".to_string())?;
    let anims = Path::new(&content).join("pilots/anims");
    let glb = std::fs::read(anims.join("pilot.glb")).map_err(|e| format!("pilot.glb: {e}"))?;
    let imported = import_gltf_animation(&glb, &GltfAnimSettings::default()).map_err(|e| e.to_string())?;
    let skin = imported.skins.into_iter().next().ok_or("pilot.glb has no skin")?;
    let skeleton = skin.skeleton;
    let mut clips = ClipLibrary::new();
    for clip in skin.clips {
        clips.insert(clip);
    }
    let text =
        std::fs::read_to_string(anims.join("body.animgraph.ron")).map_err(|e| format!("body graph: {e}"))?;
    let authored = AnimGraph::from_ron_str(&text).map_err(|e| e.to_string())?;
    let inertial = inertialized(&authored, 0.15, 0.15);
    let transitions_only = inertialized(&authored, 0.0, 0.0);
    println!(
        "pilot rig: {} joints, {} clips; body graph: {} transitions",
        skeleton.len(),
        clips.len(),
        transitions(&authored)
    );

    let scenarios: [(&str, &str, Vec<Change>); 3] = [
        (
            "walk_to_aim",
            "upperarm_r",
            vec![
                Change::Float(0.0, "Speed", 1.4),
                Change::Float(1.0, "FireWeight", 1.0),
            ],
        ),
        (
            "run_to_land",
            "thigh_l",
            vec![
                Change::Float(0.0, "Speed", 5.0),
                Change::Bool(1.0, "Air", true),
                Change::Bool(1.5, "Air", false),
            ],
        ),
        (
            "run_to_death",
            "spine_01",
            vec![Change::Float(0.0, "Speed", 5.0), Change::Bool(1.0, "Dead", true)],
        ),
    ];
    for (name, joint, changes) in &scenarios {
        let joint_index = skeleton.joint_index(joint).unwrap_or(1) as usize;
        let a = simulate(&authored, &skeleton, &clips, changes, 2.4)?;
        let b = simulate(&inertial, &skeleton, &clips, changes, 2.4)?;
        let (angle_a, speed_a) = joint_curves(&skeleton, &a, joint_index);
        let (angle_b, speed_b) = joint_curves(&skeleton, &b, joint_index);
        let mut csv = String::from(
            "time,angle_crossfade_deg,angle_inertial_deg,speed_crossfade_dps,speed_inertial_dps\n",
        );
        for i in 0..angle_a.len() {
            csv.push_str(&format!(
                "{:.4},{:.3},{:.3},{:.2},{:.2}\n",
                i as f32 * DT,
                angle_a[i],
                angle_b[i],
                speed_a[i],
                speed_b[i]
            ));
        }
        std::fs::write(out.join(format!("{name}.csv")), csv).map_err(|e| e.to_string())?;
        let markers: Vec<f32> = changes.iter().map(Change::time).filter(|t| *t > 0.0).collect();
        let window = (0.7f32, 2.4f32);
        plot(
            &out.join(format!("{name}.png")),
            &[(&angle_a, RED), (&angle_b, BLUE)],
            &[(&speed_a, RED), (&speed_b, BLUE)],
            &markers,
            window,
        )?;
        let first = markers.first().copied().unwrap_or(1.0);
        strip(
            &out.join(format!("{name}_strip.png")),
            &skeleton,
            &[&a, &b],
            first,
        )?;
        let peak = |s: &[f32]| {
            s[(window.0 / DT) as usize..]
                .iter()
                .fold(0.0f32, |m, v| m.max(*v))
        };
        println!(
            "{name} ({joint}): peak angular speed crossfade {:.0} deg/s, inertialized {:.0} deg/s",
            peak(&speed_a),
            peak(&speed_b)
        );
    }

    let mut report = format!("characters {characters}, 600 frames at 60 Hz, best of 3, single thread\n");
    // Cross-fade is timed first and again last, so an order effect (clocks, caches) shows.
    for (label, graph) in [
        ("crossfade (as authored)", &authored),
        (
            "inertialize (transitions only, authored durations)",
            &transitions_only,
        ),
        (
            "inertialize (all transitions >= 0.15 s, layer weights eased 0.15 s)",
            &inertial,
        ),
        ("crossfade (as authored, timed again)", &authored),
    ] {
        let (all, during, steady, share) = cost(graph, &skeleton, &clips, characters)?;
        let line = format!(
            "{label}: {all:.2} us/character/frame overall; {during:.2} us for a character mid-transition ({:.0} % of character-frames), {steady:.2} us otherwise\n",
            share * 100.0
        );
        print!("{line}");
        report.push_str(&line);
    }
    std::fs::write(out.join("cost.txt"), report).map_err(|e| e.to_string())?;
    Ok(())
}

/// The authored graph with every state-machine transition inertialized (at least 0.15 s) and
/// every layer's weight inertialized over 0.15 s.
fn inertialized(graph: &AnimGraph, min_duration: f32, layer_ease: f32) -> AnimGraph {
    fn walk(node: &mut AnimNode, min_duration: f32, layer_ease: f32) {
        match node {
            AnimNode::StateMachine(sm) => {
                for t in &mut sm.transitions {
                    t.blend = TransitionBlend::Inertialize;
                    // The target is in charge from the first frame, so an inertialized
                    // transition can be longer than the cross-fade it replaces.
                    t.duration = t.duration.max(min_duration);
                }
                for (_, state) in &mut sm.states {
                    walk(state, min_duration, layer_ease);
                }
            }
            AnimNode::Layer(l) => {
                walk(&mut l.base, min_duration, layer_ease);
                for layer in &mut l.layers {
                    layer.weight_inertialize = layer_ease;
                    walk(&mut layer.graph, min_duration, layer_ease);
                }
            }
            AnimNode::Blend(b) => {
                walk(&mut b.a, min_duration, layer_ease);
                walk(&mut b.b, min_duration, layer_ease);
            }
            AnimNode::Additive(a) => {
                walk(&mut a.base, min_duration, layer_ease);
                walk(&mut a.additive, min_duration, layer_ease);
            }
            AnimNode::TwoBoneIK(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::LookAt(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::RootMotion(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::Slot(n) => walk(&mut n.source, min_duration, layer_ease),
            AnimNode::ChainIK(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::BodyAim(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::StrideWarp(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::OrientationWarp(n) => walk(&mut n.inner, min_duration, layer_ease),
            AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => {}
        }
    }
    let mut g = graph.clone();
    walk(&mut g.root, min_duration, layer_ease);
    g
}

fn transitions(graph: &AnimGraph) -> usize {
    fn count(node: &AnimNode) -> usize {
        match node {
            AnimNode::StateMachine(sm) => {
                sm.transitions.len() + sm.states.iter().map(|s| count(&s.1)).sum::<usize>()
            }
            AnimNode::Layer(l) => count(&l.base) + l.layers.iter().map(|x| count(&x.graph)).sum::<usize>(),
            _ => 0,
        }
    }
    count(&graph.root)
}

/// A parameter change at a time (seconds).
enum Change {
    Float(f32, &'static str, f32),
    Bool(f32, &'static str, bool),
}

impl Change {
    fn time(&self) -> f32 {
        match self {
            Change::Float(t, ..) | Change::Bool(t, ..) => *t,
        }
    }

    fn apply(&self, params: &mut ParameterSet) {
        match self {
            Change::Float(_, name, v) => params.set_float(name, *v),
            Change::Bool(_, name, v) => params.set_bool(name, *v),
        }
    }
}

fn simulate(
    graph: &AnimGraph,
    skeleton: &Skeleton,
    clips: &ClipLibrary,
    changes: &[Change],
    seconds: f32,
) -> Result<Vec<Pose>, String> {
    let sockets = HashMap::new();
    let mut params = ParameterSet::from_decls(&graph.parameters);
    let mut instance = GraphInstance::from_node(&graph.root);
    let mut pose = Pose::from_identity(skeleton.len());
    let mut scratch = Vec::new();
    let mut poses = Vec::new();
    for frame in 0..(seconds / DT) as usize {
        let time = frame as f32 * DT;
        for c in changes {
            if (c.time() - time).abs() < DT * 0.5 {
                c.apply(&mut params);
            }
        }
        let mut ctx = EvalContext::new(
            skeleton,
            clips,
            &params,
            DT,
            Entity::PLACEHOLDER,
            &sockets,
            scratch,
        );
        evaluate_graph(graph, &mut instance, &mut ctx, &mut pose).map_err(|e| e.to_string())?;
        let consumed = std::mem::take(&mut ctx.consumed_triggers);
        scratch = ctx.finish().scratch;
        for name in &consumed {
            params.consume_trigger(name);
        }
        poses.push(pose.clone());
    }
    Ok(poses)
}

/// Per frame: the joint's local angle from its bind rotation (degrees) and its angular speed
/// (degrees per second).
fn joint_curves(skeleton: &Skeleton, poses: &[Pose], joint: usize) -> (Vec<f32>, Vec<f32>) {
    let bind = skeleton.bind_pose[joint].rotation;
    let angle = |q: Quat| q.angle_between(bind).to_degrees();
    let angles = poses.iter().map(|p| angle(p.rotations[joint])).collect();
    let mut speeds = vec![0.0];
    for w in poses.windows(2) {
        speeds.push(
            w[1].rotations[joint]
                .angle_between(w[0].rotations[joint])
                .to_degrees()
                / DT,
        );
    }
    (angles, speeds)
}

/// Mean CPU time per character per frame, overall and on frames with a transition running.
fn cost(
    graph: &AnimGraph,
    skeleton: &Skeleton,
    clips: &ClipLibrary,
    n: usize,
) -> Result<(f64, f64, f64, f64), String> {
    let sockets = HashMap::new();
    let mut best = (f64::MAX, f64::MAX, f64::MAX);
    let mut share = 0.0;
    for _ in 0..3 {
        let mut params: Vec<ParameterSet> = (0..n)
            .map(|_| ParameterSet::from_decls(&graph.parameters))
            .collect();
        let mut instances: Vec<GraphInstance> =
            (0..n).map(|_| GraphInstance::from_node(&graph.root)).collect();
        let mut poses: Vec<Pose> = (0..n).map(|_| Pose::from_identity(skeleton.len())).collect();
        let mut scratch: Vec<Vec<Pose>> = (0..n).map(|_| Vec::new()).collect();
        let (mut total, mut frames) = (0.0f64, 0usize);
        let (mut during, mut during_count) = (0.0f64, 0usize);
        let (mut steady, mut steady_count) = (0.0f64, 0usize);
        for frame in 0..600usize {
            for (i, p) in params.iter_mut().enumerate() {
                // Staggered: speed changes every 0.5 s, air toggles every 0.4 s, fire every 0.7 s.
                let phase = frame + i * 7;
                p.set_float("Speed", [0.0, 1.4, 3.0, 5.0][(phase / 30) % 4]);
                p.set_float("Strafe", if (phase / 45) % 3 == 1 { 1.5 } else { 0.0 });
                p.set_bool("Air", (phase / 24) % 3 == 2);
                p.set_float("FireWeight", if (phase / 42) % 2 == 1 { 1.0 } else { 0.0 });
            }
            let start = Instant::now();
            for i in 0..n {
                let was_running = running(&instances[i]);
                let one = Instant::now();
                let s = std::mem::take(&mut scratch[i]);
                let mut ctx =
                    EvalContext::new(skeleton, clips, &params[i], DT, Entity::PLACEHOLDER, &sockets, s);
                evaluate_graph(graph, &mut instances[i], &mut ctx, &mut poses[i])
                    .map_err(|e| e.to_string())?;
                scratch[i] = ctx.finish().scratch;
                let us = one.elapsed().as_secs_f64() * 1e6;
                // Bucket by whether a transition ran this frame (running before or after).
                if frame >= 60 {
                    if was_running || running(&instances[i]) {
                        during += us;
                        during_count += 1;
                    } else {
                        steady += us;
                        steady_count += 1;
                    }
                }
            }
            let elapsed = start.elapsed().as_secs_f64() * 1e6 / n as f64;
            if frame >= 60 {
                total += elapsed;
                frames += 1;
            }
        }
        let all = total / frames.max(1) as f64;
        let mid = during / during_count.max(1) as f64;
        let calm = steady / steady_count.max(1) as f64;
        share = during_count as f64 / (during_count + steady_count).max(1) as f64;
        best = (best.0.min(all), best.1.min(mid), best.2.min(calm));
    }
    Ok((best.0, best.1, best.2, share))
}

fn running(instance: &GraphInstance) -> bool {
    match instance {
        GraphInstance::StateMachine(sm) => sm.transition.is_some() || sm.inertia.is_active(),
        GraphInstance::Layer { base, .. } => running(base),
        GraphInstance::Wrap(inner) | GraphInstance::Warp { inner, .. } => running(inner),
        _ => false,
    }
}

// ---- tiny PNG plotting (no text; colours and layout are documented in the module doc) ----

const RED: [u8; 3] = [214, 60, 50];
const BLUE: [u8; 3] = [40, 110, 220];
const GREY: [u8; 3] = [170, 170, 170];

struct Canvas {
    image: image::RgbImage,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            image: image::RgbImage::from_pixel(w, h, image::Rgb([255, 255, 255])),
        }
    }

    fn dot(&mut self, x: i32, y: i32, c: [u8; 3], r: i32) {
        for dy in -r..=r {
            for dx in -r..=r {
                let (px, py) = (x + dx, y + dy);
                if px >= 0 && py >= 0 && (px as u32) < self.image.width() && (py as u32) < self.image.height()
                {
                    self.image.put_pixel(px as u32, py as u32, image::Rgb(c));
                }
            }
        }
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32), c: [u8; 3], r: i32) {
        let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()).ceil() as i32).max(1);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            self.dot(
                (a.0 + (b.0 - a.0) * t).round() as i32,
                (a.1 + (b.1 - a.1) * t).round() as i32,
                c,
                r,
            );
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        self.image
            .save(path)
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

type Series<'a> = (&'a Vec<f32>, [u8; 3]);

fn plot(
    path: &Path,
    top: &[Series],
    bottom: &[Series],
    markers: &[f32],
    window: (f32, f32),
) -> Result<(), String> {
    let (w, h) = (1200u32, 700u32);
    let mut c = Canvas::new(w, h);
    let panels = [(top, 20.0f32, 330.0f32), (bottom, 370.0, 680.0)];
    let x_of = |t: f32| 40.0 + (t - window.0) / (window.1 - window.0) * (w as f32 - 60.0);
    for (series, y0, y1) in panels {
        let i0 = (window.0 / DT) as usize;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for (s, _) in series {
            for v in &s[i0.min(s.len())..] {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
        if hi - lo < 1e-3 {
            hi = lo + 1.0;
        }
        let pad = (hi - lo) * 0.05;
        let (lo, hi) = (lo - pad, hi + pad);
        let y_of = |v: f32| y1 - (v - lo) / (hi - lo) * (y1 - y0);
        c.line((40.0, y1), (w as f32 - 20.0, y1), GREY, 0);
        c.line((40.0, y0), (40.0, y1), GREY, 0);
        for &m in markers {
            c.line((x_of(m), y0), (x_of(m), y1), GREY, 0);
        }
        for (s, colour) in series {
            for i in i0.max(1)..s.len() {
                let t0 = (i - 1) as f32 * DT;
                c.line(
                    (x_of(t0), y_of(s[i - 1])),
                    (x_of(t0 + DT), y_of(s[i])),
                    *colour,
                    1,
                );
            }
        }
    }
    c.save(path)
}

/// Side-view stick figures (Z → x, Y → y) every second frame from 0.1 s before `switch` to
/// 0.4 s after; one row per pose sequence.
fn strip(path: &Path, skeleton: &Skeleton, rows: &[&Vec<Pose>], switch: f32) -> Result<(), String> {
    let start = ((switch - 0.1) / DT).max(0.0) as usize;
    let frames: Vec<usize> = (start..start + 30).step_by(2).collect();
    let (cell, height) = (110.0f32, 260.0f32);
    let mut c = Canvas::new(
        (cell * frames.len() as f32) as u32,
        (height * rows.len() as f32) as u32,
    );
    let switch_frame = (switch / DT).round() as usize;
    for (row, poses) in rows.iter().enumerate() {
        let colour = if row == 0 { RED } else { BLUE };
        for (col, &f) in frames.iter().enumerate() {
            let Some(pose) = poses.get(f) else { continue };
            let globals = skeleton.globals(pose, None);
            let p: Vec<Vec3> = globals.iter().map(|m| m.w_axis.truncate()).collect();
            let ox = col as f32 * cell + cell * 0.5;
            let oy = row as f32 * height + height - 20.0;
            let scale = 120.0;
            if f >= switch_frame {
                c.line(
                    (ox - cell * 0.5, oy + 10.0),
                    (ox + cell * 0.5, oy + 10.0),
                    GREY,
                    1,
                );
            }
            for (j, parent) in skeleton.parents.iter().enumerate() {
                if let Some(parent) = parent {
                    let a = p[*parent as usize];
                    let b = p[j];
                    c.line(
                        (ox + a.z * scale, oy - a.y * scale),
                        (ox + b.z * scale, oy - b.y * scale),
                        colour,
                        1,
                    );
                }
            }
        }
    }
    c.save(path)
}
