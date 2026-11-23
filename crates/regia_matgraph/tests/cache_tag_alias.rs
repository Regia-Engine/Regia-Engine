// SPDX-License-Identifier: MPL-2.0
//! Structural cache entries can have several library owners. Forgetting any registered owner
//! invalidates the whole entry; existing `Arc`s remain usable and ownership starts fresh on a miss.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_matgraph::cache::MAX_CACHED_GRAPHS;
use regia_matgraph::{
    CacheStats, CompiledGraph, MaterialGraph, MaterialGraphLibrary, Node, ShaderCache, SwitchDecl,
    thread_compile_count,
};

fn graph(value: usize) -> MaterialGraph {
    let mut graph = MaterialGraph::new("alias");
    graph.nodes.insert("v".into(), Node::Float(value as f32));
    graph.output.roughness = Some("v".into());
    graph
}

fn lookup(cache: &mut ShaderCache, tag: Option<&str>, graph: &MaterialGraph) -> (Arc<CompiledGraph>, bool) {
    let before = thread_compile_count();
    let result = match tag {
        Some(tag) => cache.get_or_compile_tagged(tag, graph, &BTreeMap::new()),
        None => cache.get_or_compile(graph),
    }
    .expect("valid fixture");
    assert_eq!(thread_compile_count() - before, u64::from(!result.1));
    result
}

#[test]
fn aliases_share_and_forgetting_either_owner_recompiles() {
    for (first, second) in [("A", "B"), ("B", "A")] {
        for forgotten in [second, first] {
            let mut cache = ShaderCache::new();
            let graph = graph(1);
            let (original, hit) = lookup(&mut cache, Some(first), &graph);
            assert!(!hit);
            let (alias, hit) = lookup(&mut cache, Some(second), &graph);
            assert!(hit);
            assert!(Arc::ptr_eq(&original, &alias));
            assert_eq!(cache.stats().entries, 1);
            cache.forget_tag(forgotten);
            assert_eq!(
                cache.stats().entries,
                0,
                "forget {forgotten} after {first}, {second}"
            );
            let (fresh, hit) = lookup(&mut cache, Some(second), &graph);
            assert!(!hit);
            assert!(!Arc::ptr_eq(&original, &fresh));
            assert_eq!(original.key, fresh.key);
            assert_eq!(original.wgsl, fresh.wgsl, "surviving holders keep valid code");
            cache.forget_tag(first);
            assert_eq!(cache.stats().entries, 1, "old owners do not survive reinsertion");
            cache.forget_tag(second);
            assert_eq!(cache.stats().entries, 0);
        }
    }
}

#[test]
fn tagged_and_untagged_lookups_keep_registered_owners() {
    for tagged_first in [false, true] {
        let mut cache = ShaderCache::new();
        let graph = graph(2);
        let tags = if tagged_first {
            [Some("A"), None]
        } else {
            [None, Some("A")]
        };
        let (first, hit) = lookup(&mut cache, tags[0], &graph);
        assert!(!hit);
        let (second, hit) = lookup(&mut cache, tags[1], &graph);
        assert!(hit && Arc::ptr_eq(&first, &second));
        cache.forget_tag("A");
        assert_eq!(cache.stats().entries, 0);
        assert!(!lookup(&mut cache, None, &graph).1);
        cache.forget_tag("A");
        assert!(
            lookup(&mut cache, None, &graph).1,
            "untagged reinsertion has no old owner"
        );
    }
}

#[test]
fn library_aliases_invalidate_shared_but_preserve_distinct_switch_permutations() {
    for (first, second) in [("A", "B"), ("B", "A")] {
        for replace in [false, true] {
            let mut library = MaterialGraphLibrary::new();
            let mut graph = graph(3);
            graph.switches.push(SwitchDecl {
                name: "s".into(),
                default: true,
            });
            graph.nodes.insert("off".into(), Node::Float(0.25));
            graph.nodes.insert(
                "pick".into(),
                Node::Switch {
                    switch: "s".into(),
                    on: "v".into(),
                    off: "off".into(),
                },
            );
            graph.output.roughness = Some("pick".into());
            library.set(first, graph.clone());
            library.set(second, graph.clone());
            let (shared, hit) = library.compile(first).expect("name").expect("valid");
            assert!(!hit);
            let (alias, hit) = library.compile(second).expect("name").expect("valid");
            assert!(hit && Arc::ptr_eq(&shared, &alias));
            let off = BTreeMap::from([("s".to_string(), false)]);
            assert!(!library.compile_with(first, &off).expect("name").expect("valid").1);
            assert_eq!(library.cache_stats().entries, 2);
            if replace {
                library.set(second, graph);
            } else {
                assert!(library.remove(second));
            }
            assert_eq!(
                library.cache_stats().entries,
                1,
                "only the first owner's off permutation survives"
            );
            assert!(library.compile_with(first, &off).expect("name").expect("valid").1);
            let (fresh, hit) = library.compile(first).expect("name").expect("valid");
            assert!(!hit && !Arc::ptr_eq(&fresh, &shared));
            assert_eq!(fresh.key, shared.key);
        }
    }
}

#[test]
fn repeated_forget_clear_and_reinsert_reset_owners_without_resetting_counters() {
    let mut cache = ShaderCache::new();
    let graph = graph(4);
    lookup(&mut cache, Some("A"), &graph);
    for _ in 0..8 {
        assert!(lookup(&mut cache, Some("B"), &graph).1);
    }
    cache.forget_tag("B");
    assert_eq!(cache.stats().entries, 0);
    cache.forget_tag("B");
    cache.forget_tag("absent");
    lookup(&mut cache, Some("C"), &graph);
    cache.forget_tag("A");
    cache.forget_tag("B");
    assert_eq!(
        cache.stats(),
        CacheStats {
            hits: 8,
            misses: 2,
            entries: 1
        }
    );
    cache.clear();
    cache.clear();
    cache.forget_tag("C");
    assert_eq!(
        cache.stats(),
        CacheStats {
            hits: 8,
            misses: 2,
            entries: 0
        }
    );
    assert!(!lookup(&mut cache, Some("D"), &graph).1);
    cache.forget_tag("C");
    assert!(lookup(&mut cache, None, &graph).1);
    cache.forget(&graph);
    assert!(!lookup(&mut cache, None, &graph).1);
    cache.forget_tag("D");
    assert_eq!(cache.stats().entries, 1);
}

#[test]
fn failed_compiles_register_no_owner_and_success_can_later_share() {
    let mut cache = ShaderCache::new();
    let graph = graph(5);
    let mut broken = graph.clone();
    broken.output.roughness = Some("missing".into());
    assert!(
        cache
            .get_or_compile_tagged("A", &broken, &BTreeMap::new())
            .is_err()
    );
    assert_eq!(cache.stats(), CacheStats::default());
    assert!(
        !lookup(&mut cache, Some("A"), &graph).1,
        "success after failure compiles anew"
    );
    let before = cache.stats();
    let compiles = thread_compile_count();
    for _ in 0..2 {
        assert!(
            cache
                .get_or_compile_tagged("B", &broken, &BTreeMap::new())
                .is_err()
        );
    }
    assert_eq!(thread_compile_count(), compiles + 2);
    assert_eq!(
        cache.stats(),
        before,
        "ordinary compile failures preserve existing counters"
    );
    let unknown = BTreeMap::from([("unknown".to_string(), true)]);
    assert!(cache.get_or_compile_tagged("B", &graph, &unknown).is_err());
    assert_eq!(
        cache.stats().misses,
        before.misses + 1,
        "unknown-switch failures retain their existing counter rule"
    );
    cache.forget_tag("B");
    assert!(
        lookup(&mut cache, Some("A"), &graph).1,
        "failed requests cannot claim A's entry"
    );
    broken.output.roughness = graph.output.roughness.clone();
    assert!(lookup(&mut cache, Some("B"), &broken).1);
    cache.forget_tag("B");
    assert_eq!(cache.stats().entries, 0);
    assert!(!lookup(&mut cache, Some("B"), &broken).1);
}

#[test]
fn eviction_is_fifo_and_reinsertion_has_only_current_owners() {
    let mut cache = ShaderCache::new();
    let first = graph(0);
    let (held, _) = lookup(&mut cache, Some("old"), &first);
    assert!(lookup(&mut cache, Some("alias"), &first).1);
    // Prove the alias is registered before exercising eviction and stale metadata cleanup.
    cache.forget_tag("alias");
    assert_eq!(cache.stats().entries, 0);
    lookup(&mut cache, Some("old"), &first);
    lookup(&mut cache, Some("alias"), &first);
    for i in 1..=MAX_CACHED_GRAPHS {
        assert!(!lookup(&mut cache, Some("filler"), &graph(i)).1);
        assert!(cache.stats().entries <= MAX_CACHED_GRAPHS);
        if i == MAX_CACHED_GRAPHS - 1 {
            assert!(
                lookup(&mut cache, Some("alias"), &first).1,
                "hits do not refresh FIFO order"
            );
        }
    }
    let (fresh, hit) = lookup(&mut cache, Some("current"), &first);
    assert!(!hit && !Arc::ptr_eq(&held, &fresh));
    assert_eq!(held.key, fresh.key);
    cache.forget_tag("old");
    cache.forget_tag("alias");
    assert!(
        lookup(&mut cache, None, &first).1,
        "evicted owners cannot invalidate the new entry"
    );
    cache.forget_tag("filler");
    assert_eq!(cache.stats().entries, 1);
    cache.forget_tag("current");
    assert_eq!(cache.stats().entries, 0);
}

// Deliberately use a linear list of fixture IDs, independent of the cache's hash maps and keys.
#[derive(Default)]
struct Model {
    entries: Vec<(usize, Vec<&'static str>)>,
    hits: u64,
    misses: u64,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Get(usize, Option<&'static str>),
    ForgetTag(&'static str),
    Forget(usize),
    Clear,
}

impl Model {
    fn apply(&mut self, op: Op) -> Option<bool> {
        match op {
            Op::Get(id, tag) => {
                let hit = self.entries.iter().any(|(key, _)| *key == id);
                if hit {
                    self.hits += 1;
                } else {
                    self.misses += 1;
                    self.entries.push((id, Vec::new()));
                    if self.entries.len() > MAX_CACHED_GRAPHS {
                        self.entries.remove(0);
                    }
                }
                let (_, owners) = self
                    .entries
                    .iter_mut()
                    .find(|(key, _)| *key == id)
                    .expect("inserted");
                if let Some(tag) = tag
                    && !owners.contains(&tag)
                {
                    owners.push(tag);
                }
                Some(hit)
            }
            Op::ForgetTag(tag) => {
                self.entries.retain(|(_, owners)| !owners.contains(&tag));
                None
            }
            Op::Forget(id) => {
                self.entries.retain(|(key, _)| *key != id);
                None
            }
            Op::Clear => {
                self.entries.clear();
                None
            }
        }
    }
}

#[test]
fn mixed_sequences_match_a_deterministic_reference_model() {
    let mut ops = vec![Op::Get(0, Some("A")), Op::Get(0, Some("B")), Op::ForgetTag("B")];
    // Fixed enumeration exercises eviction, repeated tags, forget, and clear without randomness.
    for round in 0..3 {
        for id in 0..MAX_CACHED_GRAPHS + 5 {
            ops.push(Op::Get(id, Some("A")));
            ops.push(Op::Get(id, Some("B")));
            ops.push(Op::Get(id, None));
        }
        ops.extend([
            Op::Forget(7),
            Op::Get(7, Some("C")),
            Op::ForgetTag("A"),
            Op::ForgetTag("A"),
            Op::Get(7, None),
            Op::ForgetTag("C"),
            Op::Clear,
            Op::ForgetTag("B"),
            Op::Get(round, Some("C")),
            Op::Clear,
        ]);
    }
    let mut cache = ShaderCache::new();
    let mut model = Model::default();
    for (step, op) in ops.into_iter().enumerate() {
        let expected = model.apply(op);
        let got = match op {
            Op::Get(id, tag) => Some(lookup(&mut cache, tag, &graph(id)).1),
            Op::ForgetTag(tag) => {
                cache.forget_tag(tag);
                None
            }
            Op::Forget(id) => {
                cache.forget(&graph(id));
                None
            }
            Op::Clear => {
                cache.clear();
                None
            }
        };
        assert_eq!(got, expected, "step {step}: {op:?}");
        assert_eq!(
            cache.stats(),
            CacheStats {
                hits: model.hits,
                misses: model.misses,
                entries: model.entries.len()
            },
            "step {step}: {op:?}"
        );
    }
}
