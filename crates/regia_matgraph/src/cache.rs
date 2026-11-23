// SPDX-License-Identifier: MPL-2.0
//! The compile cache: one compiled graph per permutation hash ([`crate::permutation_hash`]: structure plus switch values; [`crate::graph_hash`] is the declared-default permutation).
//!
//! Two layers of permutation caching keep a graph from being compiled or built twice:
//! this cache maps a graph's structure to its generated code (parameter values, texture
//! paths and instances do not enter the hash), and the renderer keeps one pipeline set per
//! (render target, [`CompiledGraph::key`]) (`regia_render::surface_graph`).
//!
//! The cache holds at most [`MAX_CACHED_GRAPHS`] entries (oldest first out), and entries
//! looked up under an owner tag are dropped together by [`ShaderCache::forget_tag`]. A shared
//! structural entry records every tag that successfully looked it up, including cache hits.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::compile::{CompileErrors, CompiledGraph, compile_with, permutation_hash};
use crate::format::MaterialGraph;

/// Hit/miss counters of a [`ShaderCache`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that compiled.
    pub misses: u64,
    /// Compiled graphs held.
    pub entries: usize,
}

/// Most compiled graphs a [`ShaderCache`] keeps; the oldest permutation is dropped past it, so a
/// long edit session does not retain every shader it ever compiled.
pub const MAX_CACHED_GRAPHS: usize = 256;

/// Compiled graphs by structural hash. Failed compiles are not cached.
#[derive(Debug, Default)]
pub struct ShaderCache {
    entries: HashMap<u64, Arc<CompiledGraph>>,
    /// Hashes in insertion order (eviction order).
    order: VecDeque<u64>,
    /// Owner tags (library graph names) of each entry. Removing or replacing any owner
    /// invalidates the shared entry and every ownership association with it.
    tags: HashMap<u64, HashSet<String>>,
    hits: u64,
    misses: u64,
}

impl ShaderCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The compiled graph for `graph`, compiling (and validating) on a miss. The flag is
    /// `true` on a cache hit.
    ///
    /// # Errors
    ///
    /// [`CompileErrors`] from [`compile_with`].
    pub fn get_or_compile(
        &mut self,
        graph: &MaterialGraph,
    ) -> Result<(Arc<CompiledGraph>, bool), CompileErrors> {
        self.get_or_compile_with(graph, &BTreeMap::new())
    }

    /// [`Self::get_or_compile`] for one switch permutation (an instance's switch overrides).
    ///
    /// # Errors
    ///
    /// [`CompileErrors`] from [`compile_with`].
    pub fn get_or_compile_with(
        &mut self,
        graph: &MaterialGraph,
        switches: &BTreeMap<String, bool>,
    ) -> Result<(Arc<CompiledGraph>, bool), CompileErrors> {
        self.compile_tagged(None, graph, switches)
    }

    /// [`Self::get_or_compile_with`] that records `tag` as an owner of the entry on both
    /// hits and successful compiles, so [`Self::forget_tag`] can drop every permutation used
    /// by it. Untagged lookups preserve registered owners.
    ///
    /// # Errors
    ///
    /// [`CompileErrors`] from [`compile_with`].
    pub fn get_or_compile_tagged(
        &mut self,
        tag: &str,
        graph: &MaterialGraph,
        switches: &BTreeMap<String, bool>,
    ) -> Result<(Arc<CompiledGraph>, bool), CompileErrors> {
        self.compile_tagged(Some(tag), graph, switches)
    }

    fn compile_tagged(
        &mut self,
        tag: Option<&str>,
        graph: &MaterialGraph,
        switches: &BTreeMap<String, bool>,
    ) -> Result<(Arc<CompiledGraph>, bool), CompileErrors> {
        // An override of an undeclared switch is a compile error, not a cache hit (the
        // permutation hash only reads declared switches).
        if switches
            .keys()
            .any(|name| !graph.switches.iter().any(|s| &s.name == name))
        {
            self.misses += 1;
            return compile_with(graph, switches).map(|compiled| (Arc::new(compiled), false));
        }
        let hash = permutation_hash(graph, switches);
        let (compiled, hit) = if let Some(found) = self.entries.get(&hash) {
            self.hits += 1;
            (found.clone(), true)
        } else {
            let _span = tracing::info_span!("matgraph_compile").entered();
            let compiled = Arc::new(compile_with(graph, switches)?);
            self.misses += 1;
            self.entries.insert(hash, compiled.clone());
            self.order.push_back(hash);
            (compiled, false)
        };
        if let Some(tag) = tag {
            let owners = self.tags.entry(hash).or_default();
            if !owners.contains(tag) {
                owners.insert(tag.to_string());
            }
        }
        while self.entries.len() > MAX_CACHED_GRAPHS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
            self.tags.remove(&oldest);
        }
        Ok((compiled, hit))
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            entries: self.entries.len(),
        }
    }

    /// Drops every entry (counters stay).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.tags.clear();
    }

    /// Drops every permutation successfully looked up under `tag`.
    ///
    /// If several tags share an entry, forgetting any one invalidates it for all owners.
    /// Existing [`Arc`] holders remain valid; the next lookup recompiles and starts fresh
    /// ownership. Forgetting an absent tag is a no-op.
    pub fn forget_tag(&mut self, tag: &str) {
        let dead: Vec<u64> = self
            .tags
            .iter()
            .filter(|(_, owners)| owners.contains(tag))
            .map(|(h, _)| *h)
            .collect();
        for hash in dead {
            self.tags.remove(&hash);
            self.entries.remove(&hash);
            self.order.retain(|h| *h != hash);
        }
    }

    /// Drops the default permutation of `graph` (the entry an edit of it makes dead).
    pub fn forget(&mut self, graph: &MaterialGraph) {
        let hash = permutation_hash(graph, &BTreeMap::new());
        self.tags.remove(&hash);
        if self.entries.remove(&hash).is_some() {
            self.order.retain(|h| *h != hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Node, SwitchDecl};

    fn assert_metadata(cache: &ShaderCache) {
        let ordered: std::collections::HashSet<_> = cache.order.iter().copied().collect();
        assert_eq!(cache.order.len(), cache.entries.len());
        assert_eq!(ordered.len(), cache.entries.len(), "FIFO has no duplicates");
        assert!(cache.entries.keys().all(|hash| ordered.contains(hash)));
        assert!(cache.tags.len() <= cache.entries.len());
        assert!(cache.tags.keys().all(|hash| cache.entries.contains_key(hash)));
        assert!(cache.tags.values().all(|owners| !owners.is_empty()));
        assert!(cache.entries.len() <= MAX_CACHED_GRAPHS);
    }

    #[test]
    fn alias_metadata_is_deduplicated_and_removed_with_every_entry_lifecycle() {
        let mut cache = ShaderCache::new();
        let make_graph = |value: usize| {
            let mut graph = MaterialGraph::new("g");
            graph.nodes.insert("v".into(), Node::Float(value as f32));
            graph.output.roughness = Some("v".into());
            graph
        };
        let graph = make_graph(0);
        let hash = permutation_hash(&graph, &BTreeMap::new());
        for tag in ["A", "B", "B", "A"] {
            cache
                .get_or_compile_tagged(tag, &graph, &BTreeMap::new())
                .expect("valid");
            assert_metadata(&cache);
        }
        assert!(cache.tags[&hash].contains("B"), "hit owner is registered");
        assert_eq!(cache.tags[&hash].len(), 2, "owners are deduplicated");
        let mut broken = graph.clone();
        broken.output.roughness = Some("missing".into());
        assert!(
            cache
                .get_or_compile_tagged("C", &broken, &BTreeMap::new())
                .is_err()
        );
        assert_metadata(&cache);
        assert!(
            !cache
                .tags
                .contains_key(&permutation_hash(&broken, &BTreeMap::new()))
        );
        let unknown = BTreeMap::from([("unknown".to_string(), true)]);
        assert!(cache.get_or_compile_tagged("C", &graph, &unknown).is_err());
        assert_metadata(&cache);
        assert!(!cache.tags[&hash].contains("C"), "failed hits register no owner");
        for value in 1..=MAX_CACHED_GRAPHS + 5 {
            cache
                .get_or_compile_tagged("C", &make_graph(value), &BTreeMap::new())
                .expect("valid");
            assert_metadata(&cache);
        }
        assert!(!cache.tags.contains_key(&hash), "eviction removes every alias");
        cache.get_or_compile(&graph).expect("reinsert");
        assert_metadata(&cache);
        assert!(
            !cache.tags.contains_key(&hash),
            "untagged reinsertion has no old owners"
        );
        cache.forget_tag("A");
        cache.forget_tag("B");
        assert!(cache.entries.contains_key(&hash));
        for _ in 0..32 {
            cache
                .get_or_compile_tagged("A", &graph, &BTreeMap::new())
                .expect("valid");
            assert_metadata(&cache);
            assert_eq!(cache.tags[&hash].len(), 1);
        }
        cache.forget(&graph);
        assert!(!cache.tags.contains_key(&hash));
        assert_metadata(&cache);
        cache
            .get_or_compile_tagged("B", &graph, &BTreeMap::new())
            .expect("valid");
        cache.forget_tag("B");
        cache.forget_tag("B");
        assert_metadata(&cache);
        assert!(!cache.tags.contains_key(&hash));
        cache.clear();
        assert_metadata(&cache);
        assert!(cache.entries.is_empty() && cache.order.is_empty() && cache.tags.is_empty());
    }

    #[test]
    fn permutations_cache_separately_and_unknown_switches_never_hit() {
        let mut graph = MaterialGraph::new("g");
        graph.switches.push(SwitchDecl {
            name: "red".into(),
            default: true,
        });
        graph.nodes.insert("r".into(), Node::Vec3([1.0, 0.0, 0.0]));
        graph.nodes.insert("b".into(), Node::Vec3([0.0, 0.0, 1.0]));
        graph.nodes.insert(
            "pick".into(),
            Node::Switch {
                switch: "red".into(),
                on: "r".into(),
                off: "b".into(),
            },
        );
        graph.output.base_color = Some("pick".into());
        let mut cache = ShaderCache::new();
        let (on, hit) = cache.get_or_compile(&graph).expect("default");
        assert!(!hit);
        let mut off = BTreeMap::new();
        off.insert("red".to_string(), false);
        let (blue, hit) = cache.get_or_compile_with(&graph, &off).expect("off");
        assert!(!hit);
        assert_ne!(on.key, blue.key);
        assert!(cache.get_or_compile_with(&graph, &off).expect("again").1);
        let mut ghost = BTreeMap::new();
        ghost.insert("ghost".to_string(), true);
        assert!(cache.get_or_compile_with(&graph, &ghost).is_err());
        assert_eq!(cache.stats().entries, 2);
    }

    #[test]
    fn the_cache_is_bounded_and_forget_drops_a_permutation() {
        let mut cache = ShaderCache::new();
        let mut first = None;
        for i in 0..MAX_CACHED_GRAPHS + 10 {
            let mut graph = MaterialGraph::new("g");
            graph.nodes.insert("c".into(), Node::Float(i as f32));
            graph.nodes.insert("v".into(), Node::Vec3([i as f32, 0.0, 0.0]));
            graph.output.base_color = Some("v".into());
            graph.output.roughness = Some("c".into());
            // Constants are part of the structural hash, so each graph is a new permutation.
            cache.get_or_compile(&graph).expect("compiles");
            first.get_or_insert(graph);
        }
        assert_eq!(cache.stats().entries, MAX_CACHED_GRAPHS);
        let first = first.expect("first");
        assert!(
            !cache.get_or_compile(&first).expect("recompiles").1,
            "oldest was evicted"
        );
        cache.forget(&first);
        assert!(
            !cache.get_or_compile(&first).expect("again").1,
            "forgotten entry misses"
        );
    }
}
