// SPDX-License-Identifier: MPL-2.0
//! Shaping plant skeletons (spec 68 § 5): `plant.radius` (pipe model), `plant.bend`, `plant.lean`,
//! `plant.cut`, `plant.simplify`, `plant.scale`, `plant.twist`, `plant.attach`. Pipeline builds only.
//!
//! The pipe model follows Shinozaki et al. (1964) with the da Vinci exponent discussed by Eloy
//! (PRL 2011); `plant.bend` is a cantilever-beam sag approximation (textbook). Original code.

use regia_core::math::hash::mix;

use crate::data::{Bundle, DataKind, KindSet};
use crate::detmath;
use crate::exec::NodeCtx;
use crate::nodes::common::{Combine, Region};
use crate::nodes::plant::{
    V3, add, cross, dot, len, mul, norm_or, rotate, skeleton_item, skeleton_items, sub,
};
use crate::nodes::{
    BuiltinNode, NodeError, PinSpec, SettingType as T, Settings, eval_curve, input, kind, num, pin_in,
    pin_out, pin_req, set,
};
use crate::plant::{Plant, PlantNode};

fn pins_skel(_: &Settings) -> PinSpec {
    PinSpec::new(
        vec![pin_req("in", kind(DataKind::PlantSkeleton))],
        vec![pin_out("out", kind(DataKind::PlantSkeleton))],
    )
}

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[
    BuiltinNode::new(
        "plant.radius",
        "plant",
        "Pipe model: tips get tip_m, each parent r^e = sum of children r^e; base_swell widens the trunk foot",
        &[
            num("tip_m", T::Float, "0.004", 0.00001, 10.0, "Tip radius"),
            num("exponent", T::Float, "2.5", 1.0, 4.0, "Pipe exponent e"),
            num("trunk_scale", T::Float, "1.0", 0.01, 100.0, "Factor on every radius"),
            set("base_swell", T::Curve, "[(0.0, 1.6), (0.6, 1.0)]", "Trunk radius multiplier by height above the root"),
        ],
        pins_skel,
        eval_radius,
    )
    .plant(),
    BuiltinNode::new(
        "plant.bend",
        "plant",
        "Static sag: each branch segment rotates down about its base by min(max_deg, k M / (E r^4)), tips first",
        &[
            num("stiffness", T::Float, "1.0", 0.0001, 1.0e6, "E (relative)"),
            num("load_per_m", T::Float, "1.0", 0.0, 1.0e6, "Load per meter of carried branch"),
            num("max_deg", T::Float, "35.0", 0.0, 90.0, "Largest rotation per segment"),
        ],
        pins_skel,
        eval_bend,
    )
    .plant(),
    BuiltinNode::new(
        "plant.lean",
        "plant",
        "Leans the trunk base toward dir and straightens it by recover_height_m (subtrees rotate rigidly)",
        &[
            num("deg", T::Float, "10.0", -90.0, 90.0, "Lean at the base"),
            num("recover_height_m", T::Float, "2.0", 0.0, 1000.0, "Height by which the trunk is upright again"),
            set("dir", T::Vec3, "(1.0, 0.0, 0.0)", "Lean direction (horizontal part used)"),
        ],
        pins_skel,
        eval_lean,
    )
    .plant(),
    BuiltinNode::new(
        "plant.cut",
        "plant",
        "Removes nodes (with their subtrees) whose value is outside [min, max]; height and distance cuts shorten the crossing segment",
        &[
            set("by", T::Enum(&["height", "radius", "order", "shape", "distance"]), "height", "What is tested"),
            num("min", T::Float, "-1.0e9", -1.0e12, 1.0e12, "Lowest kept value"),
            num("max", T::Float, "1.0e9", -1.0e12, 1.0e12, "Highest kept value"),
        ],
        |_| {
            PinSpec::new(
                vec![
                    pin_req("in", kind(DataKind::PlantSkeleton)),
                    pin_in("shape", KindSet::of(&[DataKind::Shape])),
                ],
                vec![pin_out("out", kind(DataKind::PlantSkeleton))],
            )
        },
        eval_cut,
    )
    .plant(),
    BuiltinNode::new(
        "plant.simplify",
        "plant",
        "Removes straight-through nodes (turn below angle_deg) and nodes closer than min_len_m to their parent; never forks, tips or roots",
        &[
            num("angle_deg", T::Float, "4.0", 0.0, 180.0, "Turn below which a node is removed"),
            num("min_len_m", T::Float, "0.05", 0.0, 100.0, "Nodes closer to their parent are removed"),
        ],
        pins_skel,
        eval_simplify,
    )
    .plant(),
    BuiltinNode::new(
        "plant.scale",
        "plant",
        "Uniform scale of positions (about each plant's root) and radii",
        &[num("factor", T::Float, "1.0", 0.0001, 10_000.0, "Scale factor")],
        pins_skel,
        eval_scale,
    )
    .plant(),
    BuiltinNode::new(
        "plant.twist",
        "plant",
        "Alternating roll of lateral branches about their parent axis (for flat inputs)",
        &[
            num("deg_per_node", T::Float, "0.0", -360.0, 360.0, "Roll of successive lateral branches, alternating sign"),
            num("order", T::Int, "1", 1.0, 16.0, "Lowest branch order rolled"),
        ],
        pins_skel,
        eval_twist,
    )
    .plant(),
    BuiltinNode::new(
        "plant.attach",
        "plant",
        "Attaches part skeletons at sites on the base skeleton (tips, along, attr), scaled by the base radius; one skeleton out",
        &[
            set("sites", T::Enum(&["tips", "along", "attr"]), "tips", "Where parts attach"),
            num("per_m", T::Float, "1.0", 0.001, 1000.0, "Sites per meter (along)"),
            set("order", T::Range2, "[0.0, 16.0]", "Branch orders that get sites"),
            set("align", T::Enum(&["along", "up"]), "along", "Part up axis follows the branch or stays up"),
            set("scale_by_radius", T::Bool, "true", "Scale parts by site radius / part root radius"),
            set("attr", T::Name, "\"attach\"", "Custom attribute marking sites (> 0.5) for sites: attr"),
        ],
        |_| {
            PinSpec::new(
                vec![
                    pin_req("base", kind(DataKind::PlantSkeleton)),
                    pin_req("parts", kind(DataKind::PlantSkeleton)),
                ],
                vec![pin_out("out", kind(DataKind::PlantSkeleton))],
            )
        },
        eval_attach,
    )
    .plant(),
];

/// Applies `f` to every plant of every skeleton item on pin 0.
fn per_plant(
    inputs: &[Bundle],
    mut f: impl FnMut(&mut Plant) -> Result<(), NodeError>,
) -> Result<Vec<Bundle>, NodeError> {
    let mut out = Bundle::empty();
    for (item, mut sk) in skeleton_items(&input(inputs, 0)) {
        for p in &mut sk.plants {
            f(p)?;
        }
        out.0.push(skeleton_item(&item, sk));
    }
    Ok(vec![out])
}

/// Every node of the subtree rooted at `root` (children have higher indices than parents).
fn subtree(plant: &Plant, root: usize) -> Vec<usize> {
    let mut inside = vec![false; plant.nodes.len()];
    let mut out = Vec::new();
    if root >= plant.nodes.len() {
        return out;
    }
    inside[root] = true;
    out.push(root);
    for i in root + 1..plant.nodes.len() {
        if let Some(p) = plant.nodes[i].parent
            && inside[p as usize]
        {
            inside[i] = true;
            out.push(i);
        }
    }
    out
}

/// Rotates the nodes `ids` (and the directions of them and their buds) about `pivot`.
fn rotate_nodes(plant: &mut Plant, ids: &[usize], pivot: V3, axis: V3, deg: f64) {
    let mut moved = vec![false; plant.nodes.len()];
    for &i in ids {
        let n = &mut plant.nodes[i];
        n.position = add(pivot, rotate(sub(n.position, pivot), axis, deg));
        n.dir = rotate(n.dir, axis, deg);
        moved[i] = true;
    }
    for b in &mut plant.buds {
        if moved.get(b.node as usize).copied().unwrap_or(false) {
            b.dir = rotate(b.dir, axis, deg);
        }
    }
}

fn eval_radius(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let tip = s.f("tip_m");
    let e = s.f("exponent");
    let scale = s.f("trunk_scale");
    let swell = s.curve("base_swell");
    per_plant(inputs, |p| {
        pipe_radii(p, tip, e);
        let root_y = p.nodes.first().map_or(0.0, |n| n.position[1]);
        for n in &mut p.nodes {
            n.radius *= scale;
            if n.order == 0 && !swell.is_empty() {
                n.radius *= eval_curve(&swell, n.position[1] - root_y);
            }
        }
        Ok(())
    })
}

/// The pipe model: tips `tip`, every other node `(Σ r_c^e)^(1/e)` over its children.
pub(crate) fn pipe_radii(p: &mut Plant, tip: f64, e: f64) {
    let n = p.nodes.len();
    let mut sum = vec![0.0f64; n];
    let mut has_child = vec![false; n];
    for i in (0..n).rev() {
        let r = if has_child[i] {
            detmath::pow(sum[i], 1.0 / e)
        } else {
            tip
        };
        p.nodes[i].radius = r;
        if let Some(par) = p.nodes[i].parent {
            sum[par as usize] += detmath::pow(r, e);
            has_child[par as usize] = true;
        }
    }
}

fn eval_bend(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    // k: 3e-6 degrees per unit of M / (E r^4), so a 1 m branch of 2 cm radius sags about 10°.
    const K: f64 = 3.0e-6;
    let stiffness = s.f("stiffness");
    let load = s.f("load_per_m");
    let max_deg = s.f("max_deg");
    per_plant(inputs, |p| {
        let n = p.nodes.len();
        // Carried length beyond each node (its subtree's segment lengths).
        let mut carried = vec![0.0f64; n];
        for i in (1..n).rev() {
            if let Some(par) = p.nodes[i].parent {
                let seg = len(sub(p.nodes[i].position, p.nodes[par as usize].position));
                let c = carried[i] + seg;
                carried[par as usize] += c;
                carried[i] = c;
            }
        }
        for i in (1..n).rev() {
            let Some(par) = p.nodes[i].parent else {
                continue;
            };
            if p.nodes[i].order == 0 {
                continue;
            }
            let base = p.nodes[par as usize].position;
            let d = norm_or(sub(p.nodes[i].position, base), [0.0; 3]);
            let axis = cross(d, [0.0, -1.0, 0.0]);
            if len(axis) < 1e-9 {
                continue;
            }
            let axis = norm_or(axis, [1.0, 0.0, 0.0]);
            let r = p.nodes[i].radius.max(1e-4);
            let m = load * carried[i] * carried[i] * 0.5;
            let deg = (K * m / (stiffness * r * r * r * r)).min(max_deg);
            // Never past straight down.
            let to_down = detmath::acos(dot(d, [0.0, -1.0, 0.0]).clamp(-1.0, 1.0)).to_degrees();
            let deg = deg.min(to_down);
            if deg > 0.0 {
                let ids = subtree(p, i);
                rotate_nodes(p, &ids, base, axis, deg);
            }
        }
        Ok(())
    })
}

fn eval_lean(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let deg = s.f("deg");
    let recover = s.f("recover_height_m");
    let dir = s.v3("dir");
    let horiz = norm_or([dir[0], 0.0, dir[2]], [1.0, 0.0, 0.0]);
    let axis = norm_or(cross([0.0, 1.0, 0.0], horiz), [0.0, 0.0, 1.0]);
    per_plant(inputs, |p| {
        if p.nodes.is_empty() {
            return Ok(());
        }
        let root = p.nodes[0].position;
        let all: Vec<usize> = (0..p.nodes.len()).collect();
        rotate_nodes(p, &all, root, axis, deg);
        if recover <= 0.0 {
            return Ok(());
        }
        // Straighten along the trunk: each trunk segment below `recover` gives back its share.
        let mut climbed = 0.0f64;
        let mut i = 0usize;
        loop {
            let next = (i + 1..p.nodes.len())
                .find(|&c| p.nodes[c].parent == Some(i as u32) && p.nodes[c].order == 0);
            let Some(c) = next else {
                break;
            };
            let rise = (p.nodes[c].position[1] - p.nodes[i].position[1]).max(0.0);
            let give = (rise.min((recover - climbed).max(0.0)) / recover) * deg;
            climbed += rise;
            if give != 0.0 {
                let pivot = p.nodes[i].position;
                let ids = subtree(p, c);
                rotate_nodes(p, &ids, pivot, axis, -give);
            }
            i = c;
        }
        Ok(())
    })
}

fn eval_cut(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let by = s.text("by").to_string();
    let (lo, hi) = (s.f("min"), s.f("max"));
    let shape_b = input(inputs, 1);
    let region = if shape_b.0.is_empty() {
        None
    } else {
        Some(Region::new(&shape_b, Combine::Max)?)
    };
    if by == "shape" && region.is_none() {
        return Err(NodeError::new(
            "pin_unconnected",
            "plant.cut by shape needs the `shape` pin",
        ));
    }
    per_plant(inputs, |p| {
        let Some(root) = p.nodes.first().map(|n| n.position) else {
            return Ok(());
        };
        let value = |n: &PlantNode| -> f64 {
            match by.as_str() {
                "radius" => n.radius,
                "order" => f64::from(n.order),
                "shape" => region
                    .as_ref()
                    .map_or(0.0, |r| f64::from(r.value(n.position.map(|x| x as f32)))),
                "distance" => len(sub(n.position, root)),
                _ => n.position[1] - root[1],
            }
        };
        let continuous = matches!(by.as_str(), "height" | "distance");
        let n = p.nodes.len();
        let mut keep = vec![true; n];
        for i in 1..n {
            let Some(par) = p.nodes[i].parent.map(|x| x as usize) else {
                continue;
            };
            if !keep[par] {
                keep[i] = false;
                continue;
            }
            let v = value(&p.nodes[i]);
            if (lo..=hi).contains(&v) {
                continue;
            }
            let vp = value(&p.nodes[par]);
            if continuous && (lo..=hi).contains(&vp) && (v - vp).abs() > 1e-12 {
                // Shorten the crossing segment to the boundary; the node becomes a tip.
                let bound = if v > hi { hi } else { lo };
                let t = ((bound - vp) / (v - vp)).clamp(0.0, 1.0);
                let a = p.nodes[par].position;
                let b = p.nodes[i].position;
                p.nodes[i].position = add(a, mul(sub(b, a), t));
                // Its children are cut below.
                for (j, k) in keep.iter_mut().enumerate().skip(i + 1) {
                    if p.nodes[j].parent == Some(i as u32) {
                        *k = false;
                    }
                }
                continue;
            }
            keep[i] = false;
        }
        // A kept node whose parent was cut is dropped too (children follow parents in index order).
        for i in 1..n {
            if let Some(par) = p.nodes[i].parent
                && !keep[par as usize]
            {
                keep[i] = false;
            }
        }
        p.retain_nodes(&keep);
        Ok(())
    })
}

fn eval_simplify(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let angle = s.f("angle_deg");
    let min_len = s.f("min_len_m");
    per_plant(inputs, |p| {
        let n = p.nodes.len();
        let children = p.children();
        let mut has_bud = vec![false; n];
        for b in &p.buds {
            if let Some(h) = has_bud.get_mut(b.node as usize) {
                *h = true;
            }
        }
        let mut removed = vec![false; n];
        let mut new_parent: Vec<Option<u32>> = p.nodes.iter().map(|nd| nd.parent).collect();
        for i in 1..n {
            // The nearest kept ancestor.
            if let Some(par) = new_parent[i]
                && removed[par as usize]
            {
                new_parent[i] = new_parent[par as usize];
            }
            let [c] = children[i].as_slice() else {
                continue;
            };
            let c = *c as usize;
            if p.nodes[c].order != p.nodes[i].order || has_bud[i] {
                continue;
            }
            let Some(par) = new_parent[i] else {
                continue;
            };
            let a = p.nodes[par as usize].position;
            let b = p.nodes[i].position;
            let d1 = sub(b, a);
            let d2 = sub(p.nodes[c].position, b);
            let short = len(d1) < min_len;
            let turn = detmath::acos(dot(norm_or(d1, [0.0; 3]), norm_or(d2, [0.0; 3])).clamp(-1.0, 1.0))
                .to_degrees();
            if short || turn < angle {
                removed[i] = true;
            }
        }
        for (i, np) in new_parent.iter().enumerate() {
            p.nodes[i].parent = *np;
        }
        let keep: Vec<bool> = removed.iter().map(|r| !r).collect();
        p.retain_nodes(&keep);
        Ok(())
    })
}

fn eval_scale(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let f = s.f("factor");
    per_plant(inputs, |p| {
        let Some(root) = p.nodes.first().map(|n| n.position) else {
            return Ok(());
        };
        for n in &mut p.nodes {
            n.position = add(root, mul(sub(n.position, root), f));
            n.radius *= f;
        }
        Ok(())
    })
}

fn eval_twist(_: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let deg = s.f("deg_per_node");
    let min_order = u32::try_from(s.i("order")).unwrap_or(1);
    per_plant(inputs, |p| {
        if deg == 0.0 {
            return Ok(());
        }
        let n = p.nodes.len();
        let mut seen_on_parent_axis: Vec<u32> = vec![0; n];
        for i in 1..n {
            let Some(par) = p.nodes[i].parent.map(|x| x as usize) else {
                continue;
            };
            if p.nodes[i].order <= p.nodes[par].order || p.nodes[i].order < min_order {
                continue;
            }
            let k = seen_on_parent_axis[par];
            seen_on_parent_axis[par] += 1;
            let sign = if k.is_multiple_of(2) { 1.0 } else { -1.0 };
            let axis = norm_or(p.nodes[par].dir, [0.0, 1.0, 0.0]);
            let pivot = p.nodes[par].position;
            let ids = subtree(p, i);
            rotate_nodes(p, &ids, pivot, axis, sign * deg);
        }
        Ok(())
    })
}

fn eval_attach(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let sites_mode = s.text("sites").to_string();
    let per_m = s.f("per_m");
    let [olo, ohi] = s.range2("order");
    let along = s.text("align") == "along";
    let by_radius = s.b("scale_by_radius");
    let attr = s.text("attr").to_string();
    let parts: Vec<Plant> = skeleton_items(&input(inputs, 1))
        .into_iter()
        .flat_map(|(_, sk)| sk.plants)
        .filter(|p| !p.nodes.is_empty())
        .collect();
    if parts.is_empty() {
        return per_plant(inputs, |_| Ok(()));
    }
    let node_seed = ctx.node_seed;
    let mut out = Bundle::empty();
    for (item, mut sk) in skeleton_items(&input(inputs, 0)) {
        for p in &mut sk.plants {
            let children = p.children();
            // Sites: (node index, position, branch direction, radius).
            let mut sites: Vec<(u32, V3, V3, f64)> = Vec::new();
            let order_ok = |o: u32| (olo..=ohi).contains(&f64::from(o));
            for (i, nd) in p.nodes.iter().enumerate() {
                if !order_ok(nd.order) {
                    continue;
                }
                match sites_mode.as_str() {
                    "tips" => {
                        if children[i].is_empty() {
                            sites.push((i as u32, nd.position, nd.dir, nd.radius));
                        }
                    }
                    "attr" => {
                        if p.attrs
                            .get(&attr)
                            .and_then(|c| c.get(i))
                            .is_some_and(|v| *v > 0.5)
                        {
                            sites.push((i as u32, nd.position, nd.dir, nd.radius));
                        }
                    }
                    _ => {
                        let Some(par) = nd.parent else {
                            continue;
                        };
                        let a = p.nodes[par as usize].position;
                        let seg = len(sub(nd.position, a));
                        let count = (seg * per_m).floor() as u32;
                        for k in 1..=count {
                            let t = f64::from(k) / (f64::from(count) + 1.0);
                            sites.push((par, add(a, mul(sub(nd.position, a), t)), nd.dir, nd.radius));
                        }
                    }
                }
            }
            for (k, (site, pos, dir, radius)) in sites.into_iter().enumerate() {
                let pick =
                    (crate::seeds::draw(mix(p.seed, k as u32), node_seed, 0) * parts.len() as f64) as usize;
                let part = &parts[pick.min(parts.len() - 1)];
                let proot = part.nodes[0].clone();
                let scale = if by_radius && proot.radius > 0.0 {
                    radius / proot.radius
                } else {
                    1.0
                };
                // Rotation taking the part's root direction onto the branch direction.
                let (axis, angle) = if along {
                    let ax = cross(proot.dir, dir);
                    let c = dot(norm_or(proot.dir, [0.0, 1.0, 0.0]), norm_or(dir, [0.0, 1.0, 0.0]));
                    (
                        norm_or(ax, [1.0, 0.0, 0.0]),
                        detmath::acos(c.clamp(-1.0, 1.0)).to_degrees(),
                    )
                } else {
                    ([1.0, 0.0, 0.0], 0.0)
                };
                let base_order = p.nodes[site as usize].order + 1;
                let offset = p.nodes.len() as u32;
                // Part nodes after the root, re-parented under the site.
                for pn in part.nodes.iter().skip(1) {
                    let local = mul(sub(pn.position, proot.position), scale);
                    let mut nn = pn.clone();
                    nn.position = add(pos, rotate(local, axis, angle));
                    nn.dir = rotate(pn.dir, axis, angle);
                    nn.radius = pn.radius * scale;
                    nn.order = pn.order + base_order;
                    nn.parent = match pn.parent {
                        Some(0) | None => Some(site),
                        Some(q) => Some(offset + q - 1),
                    };
                    p.nodes.push(nn);
                    for col in p.attrs.values_mut() {
                        col.push(0.0);
                    }
                }
                for b in &part.buds {
                    if b.node == 0 {
                        continue;
                    }
                    let mut nb = b.clone();
                    nb.node = offset + b.node - 1;
                    nb.dir = rotate(b.dir, axis, angle);
                    nb.order = b.order + base_order;
                    p.buds.push(nb);
                }
                if p.nodes.len() > crate::nodes::plant::MAX_PLANT_NODES {
                    return Err(NodeError::new(
                        "limit_exceeded",
                        format!(
                            "plant nodes: {} > {}",
                            p.nodes.len(),
                            crate::nodes::plant::MAX_PLANT_NODES
                        ),
                    ));
                }
            }
        }
        out.0.push(skeleton_item(&item, sk));
    }
    Ok(vec![out])
}
