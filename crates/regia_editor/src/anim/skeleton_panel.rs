// SPDX-License-Identifier: MPL-2.0
//! The Skeleton viewer: the joint hierarchy, the selected joint's bind transform, sockets
//! (add on a joint, offset, rename, delete) and two-bone IK chain definitions (picked from
//! the hierarchy, used by the graph editor's IK nodes). Edits land in the rig's
//! `.animedit.ron` (undoable).

use regia_animation::authoring::IkChainDef;
use regia_animation::skeleton::{LocalBind, Skeleton, SkeletonSocket};
use regia_core::glam::EulerRot;
use regia_core::math::Quat;
use regia_ecs::prelude::*;
use regia_render::RenderDevice;

use super::{AnimEditor, PendingEdit, PreviewRig, apply_pending};

/// Children of every joint (parents are stored; the tree needs the inverse).
#[must_use]
pub fn children(skeleton: &Skeleton) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); skeleton.len()];
    for (i, parent) in skeleton.parents.iter().enumerate() {
        if let Some(p) = parent
            && let Some(list) = out.get_mut(*p as usize)
        {
            list.push(i as u32);
        }
    }
    out
}

/// A default IK chain ending at `tip` (tip, its parent, its grandparent), when the joint has
/// two ancestors.
#[must_use]
pub fn chain_ending_at(skeleton: &Skeleton, tip: u32) -> Option<IkChainDef> {
    let mid = skeleton.parents.get(tip as usize).copied().flatten()?;
    let root = skeleton.parents.get(mid as usize).copied().flatten()?;
    let name = |j: u32| skeleton.joint_names.get(j as usize).cloned().unwrap_or_default();
    Some(IkChainDef {
        name: format!("{}_ik", name(tip)),
        joints: [name(root), name(mid), name(tip)],
        pole_param: None,
    })
}

/// Draws the Skeleton panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "anim_skeleton_panel")]
pub fn anim_skeleton_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
    let _span = tracing::info_span!("anim_skeleton_panel").entered();
    if !world.contains_resource::<AnimEditor>() {
        world.init_resource::<AnimEditor>();
    }
    let mut pending = Vec::new();
    world.resource_scope(|_world, mut editor: Mut<AnimEditor>| {
        let editor = &mut *editor;
        let rig = editor.rig_or_demo().clone();
        draw(ui, editor, &rig, device, &mut pending);
    });
    apply_pending(world, pending);
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    rig: &PreviewRig,
    device: Option<&RenderDevice>,
    pending: &mut Vec<PendingEdit>,
) {
    let skeleton = rig.edited_skeleton(&editor.docs.edits);
    ui.horizontal(|ui| {
        ui.strong("Skeleton");
        ui.weak(format!(
            "{} · {} joints · {} sockets · {} IK chains",
            rig.name,
            skeleton.len(),
            skeleton.sockets.len(),
            editor.docs.edits.ik_chains.len()
        ));
    });
    egui::Panel::left("anim-skeleton-tree")
        .resizable(true)
        .default_size(220.0)
        .show(ui, |ui| {
            // Only the joints in view are laid out (ADR-7306): face rigs reach 1 000 joints.
            let kids = children(&skeleton);
            let closed_id = ui.make_persistent_id("anim-joint-closed");
            let mut closed: std::collections::HashSet<u32> =
                ui.data(|d| d.get_temp(closed_id)).unwrap_or_default();
            let rows = listed_joints(&skeleton, &kids, &closed);
            let row_height = ui.spacing().interact_size.y;
            let mut toggled = None;
            egui::ScrollArea::vertical().show_rows(ui, row_height, rows.len(), |ui, range| {
                for &(joint, depth) in &rows[range] {
                    let has_children = kids.get(joint as usize).is_some_and(|k| !k.is_empty());
                    let open = !closed.contains(&joint);
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * ui.spacing().indent);
                        if has_children {
                            if ui
                                .add(
                                    egui::Button::new(if open { "\u{25be}" } else { "\u{25b8}" })
                                        .small()
                                        .frame(false),
                                )
                                .clicked()
                            {
                                toggled = Some(joint);
                            }
                        } else {
                            ui.add_space(ui.spacing().icon_width);
                        }
                        if ui
                            .selectable_label(
                                editor.ui.selected_joint == Some(joint),
                                joint_label(&skeleton, joint),
                            )
                            .clicked()
                        {
                            editor.ui.selected_joint = Some(joint);
                        }
                    });
                }
            });
            if let Some(joint) = toggled {
                if !closed.remove(&joint) {
                    closed.insert(joint);
                }
                ui.data_mut(|d| d.insert_temp(closed_id, closed));
            }
        });
    egui::Panel::right("anim-skeleton-props")
        .resizable(true)
        .default_size(300.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| props(ui, editor, &skeleton, pending));
        });
    egui::CentralPanel::default().show(ui, |ui| {
        // Bind pose; the clip viewer shows animated poses.
        let globals = skeleton.globals(&skeleton.bind_pose_pose(), None);
        let mut orbit = editor.preview.orbit;
        if let Some(j) = super::preview::preview_widget(
            ui,
            "anim-skeleton-preview",
            &skeleton,
            &globals,
            &mut orbit,
            editor.ui.selected_joint,
            device,
        ) {
            editor.ui.selected_joint = Some(j);
        }
        editor.preview.orbit = orbit;
    });
}

/// The joints the tree lists, pre-order from every root, with their depth; a closed joint's
/// children are left out.
fn listed_joints(
    skeleton: &Skeleton,
    kids: &[Vec<u32>],
    closed: &std::collections::HashSet<u32>,
) -> Vec<(u32, usize)> {
    let mut out = Vec::with_capacity(skeleton.len());
    let mut stack: Vec<(u32, usize)> = skeleton
        .parents
        .iter()
        .enumerate()
        .filter(|(_, parent)| parent.is_none())
        .map(|(root, _)| (root as u32, 0))
        .rev()
        .collect();
    let mut seen = std::collections::HashSet::new();
    while let Some((joint, depth)) = stack.pop() {
        if !seen.insert(joint) {
            continue;
        }
        out.push((joint, depth));
        if !closed.contains(&joint)
            && let Some(children) = kids.get(joint as usize)
        {
            stack.extend(children.iter().rev().map(|child| (*child, depth + 1)));
        }
    }
    out
}

/// A joint's row label: its name and how many sockets it carries.
fn joint_label(skeleton: &Skeleton, joint: u32) -> String {
    let name = skeleton
        .joint_names
        .get(joint as usize)
        .cloned()
        .unwrap_or_default();
    let sockets = skeleton.sockets.iter().filter(|s| s.joint == name).count();
    if sockets == 0 {
        name
    } else {
        format!("{name}  \u{2693}{sockets}")
    }
}

fn props(ui: &mut egui::Ui, editor: &mut AnimEditor, skeleton: &Skeleton, pending: &mut Vec<PendingEdit>) {
    if let Some(j) = editor
        .ui
        .selected_joint
        .filter(|j| (*j as usize) < skeleton.len())
    {
        let name = skeleton.joint_names[j as usize].clone();
        let bind = skeleton.bind_pose[j as usize];
        ui.strong(format!("Joint {name} (#{j})"));
        if let Some(parent) = skeleton.parents[j as usize] {
            ui.weak(format!("parent {}", skeleton.joint_names[parent as usize]));
        }
        let t = bind.translation;
        ui.monospace(format!("bind t ({:.3}, {:.3}, {:.3})", t.x, t.y, t.z));
        let (y, x, z) = bind.rotation.to_euler(EulerRot::YXZ);
        ui.monospace(format!(
            "bind r yaw {:.1}° pitch {:.1}° roll {:.1}°",
            y.to_degrees(),
            x.to_degrees(),
            z.to_degrees()
        ));
        ui.horizontal(|ui| {
            if ui.button("Add socket here").clicked() {
                let taken: Vec<String> = skeleton.sockets.iter().map(|s| s.name.clone()).collect();
                let socket_name = (1..)
                    .map(|i| {
                        format!(
                            "{name}_socket{}",
                            if i == 1 { String::new() } else { i.to_string() }
                        )
                    })
                    .find(|n| !taken.contains(n))
                    .unwrap_or_else(|| format!("{name}_socket"));
                let joint = name.clone();
                pending.push(PendingEdit::new(
                    format!("add socket {socket_name}"),
                    None,
                    move |d| {
                        d.edits.sockets.push(SkeletonSocket {
                            name: socket_name,
                            joint,
                            offset: LocalBind::default(),
                        });
                        Ok(())
                    },
                ));
            }
            if ui.button("Add IK chain ending here").clicked() {
                match chain_ending_at(skeleton, j) {
                    Some(chain) => pending.push(PendingEdit::new(
                        format!("add IK chain {}", chain.name),
                        None,
                        move |d| {
                            d.edits.ik_chains.retain(|c| c.name != chain.name);
                            d.edits.ik_chains.push(chain);
                            Ok(())
                        },
                    )),
                    None => {
                        editor.toast = format!("{name} needs a parent and a grandparent for a two-bone chain")
                    }
                }
            }
        });
        ui.separator();
    } else {
        ui.weak("Select a joint in the tree or click one in the preview.");
        ui.separator();
    }

    ui.strong("Sockets (authored)");
    let sockets = editor.docs.edits.sockets.clone();
    let joints = skeleton.joint_names.clone();
    for (i, socket) in sockets.iter().enumerate() {
        egui::CollapsingHeader::new(format!("{} on {}", socket.name, socket.joint))
            .id_salt(("anim-socket", i))
            .show(ui, |ui| {
                let mut s = socket.clone();
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("Name");
                    changed |= ui.text_edit_singleline(&mut s.name).changed();
                });
                egui::ComboBox::from_id_salt(("anim-socket-joint", i))
                    .selected_text(s.joint.clone())
                    .show_ui(ui, |ui| {
                        for j in &joints {
                            changed |= ui.selectable_value(&mut s.joint, j.clone(), j).changed();
                        }
                    });
                ui.horizontal(|ui| {
                    ui.label("Offset");
                    for axis in 0..3 {
                        changed |= ui
                            .add(egui::DragValue::new(&mut s.offset.translation[axis]).speed(0.005))
                            .changed();
                    }
                });
                let (mut yaw, mut pitch, mut roll) = s.offset.rotation.to_euler(EulerRot::YXZ);
                let mut deg = [yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees()];
                ui.horizontal(|ui| {
                    ui.label("Rotation°");
                    let mut rot = false;
                    for v in &mut deg {
                        rot |= ui.add(egui::DragValue::new(v).speed(0.5)).changed();
                    }
                    if rot {
                        (yaw, pitch, roll) = (deg[0].to_radians(), deg[1].to_radians(), deg[2].to_radians());
                        s.offset.rotation = Quat::from_euler(EulerRot::YXZ, yaw, pitch, roll);
                        changed = true;
                    }
                });
                if changed {
                    pending.push(PendingEdit::new(
                        "edit socket",
                        Some(format!("socket:{i}")),
                        move |d| {
                            let slot = d
                                .edits
                                .sockets
                                .get_mut(i)
                                .ok_or_else(|| format!("no socket {i}"))?;
                            *slot = s;
                            Ok(())
                        },
                    ));
                }
                if ui.button("Delete socket").clicked() {
                    pending.push(PendingEdit::new("delete socket", None, move |d| {
                        if i < d.edits.sockets.len() {
                            d.edits.sockets.remove(i);
                            Ok(())
                        } else {
                            Err(format!("no socket {i}"))
                        }
                    }));
                }
            });
    }
    let imported: Vec<&SkeletonSocket> = skeleton
        .sockets
        .iter()
        .filter(|s| !sockets.iter().any(|a| a.name == s.name))
        .collect();
    if !imported.is_empty() {
        ui.weak("Imported sockets:");
        for s in imported {
            ui.weak(format!("  {} on {}", s.name, s.joint));
        }
    }

    ui.separator();
    ui.strong("IK chains");
    let chains = editor.docs.edits.ik_chains.clone();
    for (i, chain) in chains.iter().enumerate() {
        egui::CollapsingHeader::new(format!(
            "{}: {} -> {} -> {}",
            chain.name, chain.joints[0], chain.joints[1], chain.joints[2]
        ))
        .id_salt(("anim-chain", i))
        .show(ui, |ui| {
            let mut c = chain.clone();
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("Name");
                changed |= ui.text_edit_singleline(&mut c.name).changed();
            });
            for (slot, label) in ["Root", "Mid", "Tip"].iter().enumerate() {
                egui::ComboBox::from_id_salt(("anim-chain-joint", i, slot))
                    .selected_text(format!("{label}: {}", c.joints[slot]))
                    .show_ui(ui, |ui| {
                        for j in &joints {
                            changed |= ui.selectable_value(&mut c.joints[slot], j.clone(), j).changed();
                        }
                    });
            }
            let mut pole = c.pole_param.clone().unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label("Pole param");
                if ui.text_edit_singleline(&mut pole).changed() {
                    c.pole_param = (!pole.is_empty()).then_some(pole.clone());
                    changed = true;
                }
            });
            if valid_chain(skeleton, &c) {
                ui.weak("valid chain (each joint is the parent of the next)");
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 150, 90),
                    "joints are not a parent-to-child chain",
                );
            }
            if changed {
                pending.push(PendingEdit::new(
                    "edit IK chain",
                    Some(format!("chain:{i}")),
                    move |d| {
                        let slot = d
                            .edits
                            .ik_chains
                            .get_mut(i)
                            .ok_or_else(|| format!("no chain {i}"))?;
                        *slot = c;
                        Ok(())
                    },
                ));
            }
            if ui.button("Delete chain").clicked() {
                pending.push(PendingEdit::new("delete IK chain", None, move |d| {
                    if i < d.edits.ik_chains.len() {
                        d.edits.ik_chains.remove(i);
                        Ok(())
                    } else {
                        Err(format!("no chain {i}"))
                    }
                }));
            }
        });
    }
}

/// Whether the chain's joints exist and each is the parent of the next.
#[must_use]
pub fn valid_chain(skeleton: &Skeleton, chain: &IkChainDef) -> bool {
    let ids: Option<Vec<u32>> = chain.joints.iter().map(|n| skeleton.joint_index(n)).collect();
    let Some(ids) = ids else {
        return false;
    };
    skeleton.parents.get(ids[1] as usize).copied().flatten() == Some(ids[0])
        && skeleton.parents.get(ids[2] as usize).copied().flatten() == Some(ids[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_and_chain_helpers() {
        let rig = PreviewRig::demo();
        let kids = children(&rig.skeleton);
        let pelvis = rig.skeleton.joint_index("pelvis").expect("pelvis");
        assert!(kids[pelvis as usize].len() >= 3, "spine + two thighs");
        let foot = rig.skeleton.joint_index("foot_l").expect("foot");
        let chain = chain_ending_at(&rig.skeleton, foot).expect("chain");
        assert_eq!(
            chain.joints,
            ["thigh_l".to_string(), "shin_l".into(), "foot_l".into()]
        );
        assert!(valid_chain(&rig.skeleton, &chain));
        let mut broken = chain.clone();
        broken.joints[0] = "head".into();
        assert!(!valid_chain(&rig.skeleton, &broken));
        assert!(chain_ending_at(&rig.skeleton, 0).is_none());
    }

    #[test]
    fn listed_joints_walk_pre_order_and_skip_closed_children() {
        let rig = PreviewRig::demo();
        let skeleton = &rig.skeleton;
        let kids = children(skeleton);
        let open = listed_joints(skeleton, &kids, &std::collections::HashSet::new());
        assert_eq!(open.len(), skeleton.len(), "every joint listed when all are open");
        for (i, (joint, depth)) in open.iter().enumerate() {
            match skeleton.parents[*joint as usize] {
                None => assert_eq!(*depth, 0),
                Some(parent) => {
                    let at = open[..i]
                        .iter()
                        .position(|(j, _)| *j == parent)
                        .expect("parent listed first");
                    assert_eq!(*depth, open[at].1 + 1);
                }
            }
        }
        let pelvis = skeleton.joint_index("pelvis").expect("pelvis");
        let closed = listed_joints(skeleton, &kids, &std::collections::HashSet::from([pelvis]));
        let foot = skeleton.joint_index("foot_l").expect("foot");
        assert!(closed.iter().any(|(j, _)| *j == pelvis));
        assert!(
            !closed.iter().any(|(j, _)| *j == foot),
            "a closed joint hides its subtree"
        );
    }
}
