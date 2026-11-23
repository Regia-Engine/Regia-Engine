// SPDX-License-Identifier: MPL-2.0
//! Transient aliasing: transients with disjoint lifetimes share one pooled texture (ADR-2074,
//! `specs/22` T2.12).
//!
//! wgpu has no placed resources, so two textures never share memory. What a graph can do is use
//! one texture for two resources: two transients with an equal [`TextureDesc`] whose lifetimes
//! do not overlap need only one allocation. A resource's lifetime runs from its first writer to
//! its last reader in the compiled order. [`RenderGraph::alias_plan`] assigns the slots with the
//! left-edge rule, the classic optimal colouring of an interval graph (Hashimoto and Stevens,
//! 1971): take the intervals by start and give each the first slot free by then; it uses as many
//! slots as the most lifetimes that overlap at one point.
//!
//! [`RenderGraph::execute`] follows the same rule when [`TransientPool::set_aliasing`] is on: it
//! releases a transient after its last reader, and the pool's first-free search hands the texture
//! to the next acquire of that description. A texture's contents are undefined at its first
//! writer then, so that writer clears it or overwrites all of it: the graph cannot see a writer
//! that loads what is there, or one that returns `Ok` without writing, and its reader would see
//! the earlier resource's pixels. A writer that needs last frame's contents declares its own
//! resource as an input, and that writer is what samples last frame. An earlier reader still
//! gets no view. Two kinds of transient keep a texture of their own: one whose first writer
//! also lists it as an input, and one in `keep_outputs`
//! (the caller reads it after the run).
//!
//! The rule is off unless asked for, and with it off a graph runs as before: one texture per name
//! for the whole run.

use std::collections::HashMap;

use super::{GraphCompileError, GraphResource, RenderGraph, TextureDesc};

/// One transient texture's lifetime in a compiled graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientLifetime {
    /// Resource name.
    pub name: String,
    /// Its description.
    pub desc: TextureDesc,
    /// Index into the execution order of its first writer.
    pub first: usize,
    /// Index into the execution order of its last reader or writer. The caller reading it after
    /// the run ([`Self::aliasable`] false) keeps it to the end of the order.
    pub last: usize,
    /// It may share a texture: no node reads it before the first write, and it is not a kept
    /// output.
    pub aliasable: bool,
    /// The texture it gets, an index into [`AliasPlan::slots`].
    pub slot: usize,
}

/// [`RenderGraph::alias_plan`]: which transient uses which texture.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AliasPlan {
    /// Every transient of the live nodes, by first writer.
    pub lifetimes: Vec<TransientLifetime>,
    /// The description of each texture the plan needs.
    pub slots: Vec<TextureDesc>,
    /// Bytes with one texture per transient.
    pub bytes_separate: u64,
    /// Bytes of [`Self::slots`].
    pub bytes_aliased: u64,
}

impl RenderGraph {
    /// The transients of the graph's live nodes, their lifetimes in the compiled order and the
    /// textures they need with aliasing on (see the module docs). A resource declared
    /// [`GraphResource::Imported`] is not part of the plan. The plan does not know what a caller
    /// imports at [`Self::execute`], so a name declared a transient and imported anyway still has
    /// a slot here and takes no texture there.
    ///
    /// # Errors
    ///
    /// As [`Self::try_compile`].
    pub fn alias_plan(&self, keep_outputs: &[&str]) -> Result<AliasPlan, GraphCompileError> {
        let order = self.try_compile(keep_outputs)?;
        let mut lifetimes: Vec<TransientLifetime> = Vec::new();
        let mut index: HashMap<&str, usize> = HashMap::new();
        // Names a live node read before any live node wrote them.
        let mut read_first: Vec<&str> = Vec::new();
        for (step, &node) in order.iter().enumerate() {
            let node = &self.nodes[node];
            for input in &node.inputs {
                match index.get(input.name.as_str()) {
                    Some(&at) => lifetimes[at].last = step,
                    None => read_first.push(input.name.as_str()),
                }
            }
            for out in &node.outputs {
                let GraphResource::Texture(desc) = &out.desc else {
                    continue;
                };
                match index.get(out.name.as_str()) {
                    Some(&at) => lifetimes[at].last = step,
                    None => {
                        index.insert(out.name.as_str(), lifetimes.len());
                        lifetimes.push(TransientLifetime {
                            name: out.name.clone(),
                            desc: desc.clone(),
                            first: step,
                            last: step,
                            aliasable: true,
                            slot: 0,
                        });
                    }
                }
            }
        }
        for lifetime in &mut lifetimes {
            let name = lifetime.name.as_str();
            let kept = keep_outputs.contains(&name);
            if kept || read_first.contains(&name) {
                lifetime.aliasable = false;
                lifetime.last = order.len();
            }
        }
        // Left-edge: `lifetimes` is sorted by first writer. Per slot, the last step it is in use;
        // a slot taken by a transient that keeps its texture is never free again.
        let mut slots: Vec<TextureDesc> = Vec::new();
        let mut busy_until: Vec<usize> = Vec::new();
        for lifetime in &mut lifetimes {
            let free = lifetime
                .aliasable
                .then(|| {
                    (0..slots.len())
                        .find(|&slot| slots[slot] == lifetime.desc && busy_until[slot] < lifetime.first)
                })
                .flatten();
            lifetime.slot = free.unwrap_or_else(|| {
                slots.push(lifetime.desc.clone());
                busy_until.push(0);
                slots.len() - 1
            });
            busy_until[lifetime.slot] = lifetime.last;
        }
        Ok(AliasPlan {
            bytes_separate: lifetimes.iter().map(|l| l.desc.bytes()).sum(),
            bytes_aliased: slots.iter().map(TextureDesc::bytes).sum(),
            lifetimes,
            slots,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GraphInput, GraphOutput, RenderGraphNode};
    use super::*;

    const DESC: TextureDesc = TextureDesc::new(64, 64, wgpu::TextureFormat::Rgba8Unorm);
    const OTHER: TextureDesc = TextureDesc::new(64, 64, wgpu::TextureFormat::Rgba16Float);

    fn node(id: &str, reads: &[&str], writes: &[(&str, TextureDesc)]) -> RenderGraphNode {
        RenderGraphNode {
            id: id.into(),
            inputs: reads
                .iter()
                .map(|name| GraphInput {
                    name: (*name).to_string(),
                })
                .collect(),
            outputs: writes
                .iter()
                .map(|(name, desc)| GraphOutput {
                    name: (*name).to_string(),
                    desc: GraphResource::Texture(desc.clone()),
                })
                .collect(),
            slot: None,
            pipelines: Vec::new(),
            record: Box::new(|_| Ok(())),
        }
    }

    fn imported_out(id: &str, reads: &[&str], name: &str) -> RenderGraphNode {
        let mut n = node(id, reads, &[]);
        n.outputs.push(GraphOutput {
            name: name.into(),
            desc: GraphResource::Imported,
        });
        n
    }

    fn slot_of(plan: &AliasPlan, name: &str) -> usize {
        plan.lifetimes
            .iter()
            .find(|l| l.name == name)
            .map(|l| l.slot)
            .unwrap_or_else(|| panic!("{name} is not in the plan"))
    }

    #[test]
    fn disjoint_lifetimes_of_one_description_share_a_texture() {
        let mut g = RenderGraph::versioned();
        g.add_node(node("a", &[], &[("x", DESC)]));
        g.add_node(imported_out("read_x", &["x"], "c"));
        g.add_node(node("b", &["c"], &[("y", DESC)]));
        g.add_node(imported_out("read_y", &["y", "c"], "output"));
        let plan = g.alias_plan(&["output"]).expect("plan");
        assert_eq!(plan.slots.len(), 1, "{plan:#?}");
        assert_eq!(slot_of(&plan, "x"), slot_of(&plan, "y"));
        assert_eq!(plan.bytes_separate, plan.bytes_aliased * 2);
    }

    #[test]
    fn overlapping_lifetimes_and_other_descriptions_do_not() {
        let mut g = RenderGraph::versioned();
        // A blur: the raw image and the blurred one are both live in `blur`.
        g.add_node(node("kernel", &[], &[("raw", DESC)]));
        g.add_node(node("blur", &["raw"], &[("soft", DESC)]));
        // Free by now, but another format.
        g.add_node(node("other", &["soft"], &[("hdr", OTHER)]));
        g.add_node(imported_out("end", &["hdr"], "output"));
        let plan = g.alias_plan(&["output"]).expect("plan");
        assert_eq!(plan.slots.len(), 3, "{plan:#?}");
        assert_eq!(plan.bytes_separate, plan.bytes_aliased);
    }

    #[test]
    fn a_lifetime_starting_where_another_ends_is_an_overlap() {
        // `b` reads `x` while it writes `y`: one node, both textures bound.
        let mut g = RenderGraph::versioned();
        g.add_node(node("a", &[], &[("x", DESC)]));
        g.add_node(node("b", &["x"], &[("y", DESC)]));
        g.add_node(node("c", &["y"], &[("z", DESC)]));
        g.add_node(imported_out("end", &["z"], "output"));
        let plan = g.alias_plan(&["output"]).expect("plan");
        assert_eq!(plan.slots.len(), 2, "{plan:#?}");
        assert_ne!(slot_of(&plan, "x"), slot_of(&plan, "y"));
        assert_eq!(
            slot_of(&plan, "x"),
            slot_of(&plan, "z"),
            "x is free before z's writer"
        );
    }

    #[test]
    fn the_plan_uses_as_many_textures_as_the_most_that_overlap() {
        // Staggered lifetimes: at most two are alive at once, whatever the count.
        let names: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
        let mut g = RenderGraph::versioned();
        g.add_node(node("n0", &[], &[(&names[0], DESC)]));
        for i in 1..names.len() {
            g.add_node(node(&format!("n{i}"), &[&names[i - 1]], &[(&names[i], DESC)]));
        }
        g.add_node(imported_out("end", &[&names[7]], "output"));
        let plan = g.alias_plan(&["output"]).expect("plan");
        assert_eq!(plan.slots.len(), 2);
        assert_eq!(plan.bytes_separate, plan.bytes_aliased * 4);
    }

    #[test]
    fn last_frames_contents_and_kept_outputs_keep_their_texture() {
        let mut g = RenderGraph::versioned();
        // `history` is read before it is written: it holds the last frame's image.
        g.add_node(node("resolve", &["history"], &[("resolved", DESC)]));
        g.add_node(node("save", &["resolved"], &[("history", DESC)]));
        // `resolved` is over by now, but a kept output takes a texture of its own from the start
        // of the run: it reuses nothing and nothing reuses it.
        g.add_node(node("show", &["history"], &[("final", DESC)]));
        g.add_node(node("later", &["history"], &[("late", OTHER)]));
        let plan = g.alias_plan(&["final", "late"]).expect("plan");
        let get = |name: &str| plan.lifetimes.iter().find(|l| l.name == name).expect("in plan");
        assert!(!get("history").aliasable);
        assert!(!get("final").aliasable);
        assert!(get("resolved").aliasable);
        assert_ne!(slot_of(&plan, "history"), slot_of(&plan, "resolved"));
        assert_ne!(slot_of(&plan, "final"), slot_of(&plan, "resolved"));
        assert_eq!(plan.slots.len(), 4);
    }

    #[test]
    fn a_culled_nodes_transient_is_not_in_the_plan_and_imports_never_are() {
        let mut g = RenderGraph::versioned();
        g.add_node(node("dead", &[], &[("unused", DESC)]));
        g.add_node(imported_out("end", &[], "output"));
        let plan = g.alias_plan(&["output"]).expect("plan");
        assert!(plan.lifetimes.is_empty());
        assert!(plan.slots.is_empty());
        assert_eq!(plan.bytes_aliased, 0);
    }
}
