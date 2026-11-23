// SPDX-License-Identifier: MPL-2.0
//! The live set of material graphs an app (editor, agent, game) works on, as an ECS resource.
//! The agent methods `material.graph_get`, `material.graph_set` and `material.compile` read and
//! write it (`book/agents/material-graph.md`).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;

use crate::cache::{CacheStats, ShaderCache};
use crate::compile::{CompileErrors, CompiledGraph};
use crate::format::MaterialGraph;

/// One graph in the library.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryEntry {
    /// The graph as last set.
    pub graph: MaterialGraph,
    /// Bumped on every change (any graph in the library), so a client can tell edits apart.
    pub revision: u64,
}

/// Named material graphs plus the compile cache. Inserted by [`MaterialGraphPlugin`].
#[derive(Resource, Debug, Default)]
pub struct MaterialGraphLibrary {
    entries: BTreeMap<String, LibraryEntry>,
    cache: ShaderCache,
    revision: u64,
}

impl MaterialGraphLibrary {
    /// An empty library.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces `name`; returns the new revision.
    pub fn set(&mut self, name: impl Into<String>, graph: MaterialGraph) -> u64 {
        self.revision += 1;
        let revision = self.revision;
        let name = name.into();
        // The edit makes every compiled permutation of the old graph dead weight.
        self.cache.forget_tag(&name);
        self.entries.insert(name, LibraryEntry { graph, revision });
        revision
    }

    /// The entry for `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&LibraryEntry> {
        self.entries.get(name)
    }

    /// Removes `name`; true when it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.cache.forget_tag(name);
        self.entries.remove(name).is_some()
    }

    /// Graph names in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Compiles `name` through the cache (`None` when there is no such graph). The flag is
    /// `true` on a cache hit.
    pub fn compile(&mut self, name: &str) -> Option<Result<(Arc<CompiledGraph>, bool), CompileErrors>> {
        self.compile_with(name, &std::collections::BTreeMap::new())
    }

    /// [`Self::compile`] for one static-switch permutation.
    pub fn compile_with(
        &mut self,
        name: &str,
        switches: &std::collections::BTreeMap<String, bool>,
    ) -> Option<Result<(Arc<CompiledGraph>, bool), CompileErrors>> {
        let graph = &self.entries.get(name)?.graph;
        Some(self.cache.get_or_compile_tagged(name, graph, switches))
    }

    /// Compile-cache counters.
    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }
}

/// Material graphs in an app: the [`MaterialGraphLibrary`] the agent edits, the
/// [`crate::GraphMaterials`] runtime (cooked graph assets on [`crate::MaterialGraphRef`]
/// entities, hot reload, pipeline prewarm) and the material clock.
#[derive(Debug, Default)]
pub struct MaterialGraphPlugin;

impl Plugin for MaterialGraphPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MaterialGraphLibrary>()
            .init_resource::<crate::GraphMaterials>()
            .register_type::<crate::MaterialGraphRef>()
            .add_message::<regia_scene::level::LevelLoaded>()
            .add_systems(
                PostUpdate,
                (
                    (crate::apply_material_graphs, crate::prewarm_graph_pipelines).chain(),
                    crate::drive_material_clock,
                ),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Node;

    #[test]
    fn revisions_advance_and_entries_replace() {
        let mut library = MaterialGraphLibrary::new();
        let mut graph = MaterialGraph::new("a");
        graph.nodes.insert("c".into(), Node::Float(0.3));
        graph.output.roughness = Some("c".into());
        assert_eq!(library.set("a", graph.clone()), 1);
        assert_eq!(library.set("a", graph), 2);
        assert_eq!(library.names().collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(library.get("a").map(|e| e.revision), Some(2));
        assert!(library.compile("missing").is_none());
        assert!(library.remove("a"));
        assert!(!library.remove("a"));
    }

    #[test]
    fn removing_or_replacing_a_graph_drops_all_its_permutations() {
        use crate::format::SwitchDecl;
        let mut library = MaterialGraphLibrary::new();
        let mut graph = MaterialGraph::new("scratch");
        graph.switches.push(SwitchDecl {
            name: "s".into(),
            default: true,
        });
        graph.nodes.insert("a".into(), Node::Float(1.0));
        graph.nodes.insert("b".into(), Node::Float(0.0));
        graph.nodes.insert(
            "pick".into(),
            Node::Switch {
                switch: "s".into(),
                on: "a".into(),
                off: "b".into(),
            },
        );
        graph.output.roughness = Some("pick".into());
        library.set("scratch", graph.clone());
        for on in [true, false] {
            let switches = std::collections::BTreeMap::from([("s".to_string(), on)]);
            assert!(
                library
                    .compile_with("scratch", &switches)
                    .is_some_and(|r| r.is_ok())
            );
        }
        assert_eq!(library.cache_stats().entries, 2);
        library.set("scratch", graph.clone());
        assert_eq!(library.cache_stats().entries, 0);
        let _ = library.compile("scratch");
        assert_eq!(library.cache_stats().entries, 1);
        assert!(library.remove("scratch"));
        assert_eq!(library.cache_stats().entries, 0);
    }
}
