// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (sprint 2026-10-21): motion-matching paths no other test reaches.
//!
//! * `KdTree::with_rows` (the per-dimension rescale reuse) and the leaf-size sweep against a
//!   linear scan, on fixed-seed data.
//! * `nearest_k_biased` with ties and with `k` above the eligible rows.
//! * `load_motion_database_file` dependency failures (missing clip, damaged skeleton, wrong
//!   dependency count, truncated container) and the `MotionDatabases` path fallbacks.

use regia_animation::cooked::{clip_type_tag, motion_database_type_tag, skeleton_type_tag};
use regia_animation::motion_matching::procedural::procedural_locomotion;
use regia_animation::motion_matching::registry::{
    MotionDatabaseAssetError, MotionDatabases, load_motion_database_file,
};
use regia_animation::motion_matching::{
    KdTree, LoadedMotionDatabase, nearest_brute_force, nearest_brute_force_biased,
};
use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version};
use regia_core::id::TypePathHash;

/// A tiny deterministic generator (xorshift) so the data is fixed without a dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

fn rows(seed: u64, count: usize, dims: usize) -> Vec<f32> {
    let mut rng = Lcg(seed);
    (0..count * dims).map(|_| rng.next() * 4.0 - 2.0).collect()
}

#[test]
fn with_rows_after_a_per_dimension_rescale_matches_a_linear_scan() {
    let dims = 5;
    let base = rows(0x51ED, 600, dims);
    let tree = KdTree::build(&base, dims).expect("tree");
    // Non-negative factors keep every dimension non-decreasing, the documented exact case; a
    // zero factor collapses a dimension entirely.
    let factors = [0.5f32, 2.0, 0.0, 3.25, 1.0];
    let scaled: Vec<f32> = base
        .chunks_exact(dims)
        .flat_map(|r| r.iter().zip(factors).map(|(v, f)| v * f))
        .collect();
    let reused = tree.with_rows(scaled.clone()).expect("same shape");
    let mut rng = Lcg(99);
    for q in 0..200usize {
        let query: Vec<f32> = (0..dims).map(|_| rng.next() * 4.0 - 2.0).collect();
        let skip = q % 7;
        let excluded = |row: usize| row % 7 == skip;
        let want = nearest_brute_force(&scaled, dims, &query, excluded);
        let got = reused.nearest(&query, excluded);
        assert_eq!(got, want, "query {q}");
    }
    assert_eq!(tree.with_rows(vec![0.0; 3]), None, "a different shape is refused");
}

#[test]
fn every_leaf_size_finds_the_linear_scan_answer() {
    let dims = 3;
    let data = rows(7, 257, dims);
    let mut rng = Lcg(5);
    let queries: Vec<Vec<f32>> = (0..60)
        .map(|_| (0..dims).map(|_| rng.next() * 4.0 - 2.0).collect())
        .collect();
    // In-range requests only: `build_with_leaf_size` clamps to `LEAF_SIZE_RANGE` (4..=32), so
    // 1/2/3 would all build leaf size 4 and 64/1000 would build 32, sweeping three sizes under
    // this test's name.
    for leaf in [4usize, 5, 8, 16, 31, 32] {
        let tree = KdTree::build_with_leaf_size(&data, dims, leaf).expect("tree");
        assert_eq!(tree.leaf_size(), leaf, "an in-range request is kept");
        for (i, query) in queries.iter().enumerate() {
            let bias = |row: usize| (row % 5) as f32 * 0.01;
            assert_eq!(
                tree.nearest(query, |_| false),
                nearest_brute_force(&data, dims, query, |_| false),
                "leaf {leaf}, query {i}"
            );
            assert_eq!(
                tree.nearest_biased(query, |_| false, bias, 0.0),
                nearest_brute_force_biased(&data, dims, query, |_| false, bias),
                "leaf {leaf}, biased query {i}"
            );
        }
    }
}

#[test]
fn k_best_orders_ties_by_row_and_stops_at_the_eligible_rows() {
    // Rows 0, 1, 3, 4 are identical and row 2 is farther: every tie is broken by row index.
    let data = [1.0f32, 1.0, 1.0, 1.0, 5.0, 1.0, 1.0, 1.0, 1.0, 1.0];
    let tree = KdTree::build(&data, 2).expect("tree");
    let query = [1.0f32, 1.0];
    let (top, _) = tree.nearest_k_biased(&query, |row| row == 1, |_| 0.0, 0.0, 10);
    let rows: Vec<usize> = top.iter().map(|n| n.row).collect();
    assert_eq!(
        rows,
        vec![0, 3, 4, 2],
        "row 1 excluded, ties by index, the far row last"
    );
    assert_eq!(top[0].distance, 0.0);
    assert_eq!(top[3].distance, 16.0);
    let (none, _) = tree.nearest_k_biased(&query, |_| false, |_| 0.0, 0.0, 0);
    assert!(none.is_empty(), "k = 0 finds nothing");
    // A bias reorders the equal rows: the last tie gets the lowest cost and wins.
    let (top, _) = tree.nearest_k_biased(
        &query,
        |_| false,
        |row| if row == 3 { -0.5 } else { 0.0 },
        -0.5,
        2,
    );
    assert_eq!(top.iter().map(|n| n.row).collect::<Vec<_>>(), vec![3, 0]);
}

#[test]
fn k_best_visits_the_far_side_for_equal_cost_rows_in_other_leaves() {
    // Eight rows alternating (0,0) and (10,10) with leaf size 4: the split on x (both axes tie
    // at spread 10, the lowest dimension wins) puts the even rows in one leaf and the odd rows
    // in the other. The (5,5) query is equidistant (50) from every row, so the true top-4 by
    // `(distance, row)` is [0, 1, 2, 3] only when the far leaf is visited: a search that drops
    // the far side returns whichever leaf it scanned first.
    let mut data = Vec::new();
    for i in 0..8 {
        let v = if i % 2 == 0 { 0.0 } else { 10.0 };
        data.extend([v, v]);
    }
    let tree = KdTree::build_with_leaf_size(&data, 2, 4).expect("tree");
    assert!(
        tree.len() == 8 && tree.leaf_size() == 4,
        "the fixture spans more than one leaf (8 rows, leaf size 4)"
    );
    let query = [5.0f32, 5.0];
    let (top, work) = tree.nearest_k_biased(&query, |_| false, |_| 0.0, 0.0, 4);
    assert_eq!(
        top.iter().map(|n| n.row).collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "ties across the split resolve by row index only with the far side visited"
    );
    assert_eq!(work.nodes_visited, 3, "the root and both leaves are visited");
    // Excluding the would-be winners changes the answer only through the far leaf's rows.
    let (top, _) = tree.nearest_k_biased(&query, |row| row < 4, |_| 0.0, 0.0, 4);
    assert_eq!(top.iter().map(|n| n.row).collect::<Vec<_>>(), vec![4, 5, 6, 7]);
}

/// A cooked file of `tag` carrying `payload`, depending on `dependencies`.
fn cooked(tag: TypePathHash, payload: &[u8], id: AssetId, dependencies: Vec<AssetId>) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: tag,
        asset_id: id,
        importer_version: pack_importer_version(0, 2, 5),
        dependencies,
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode")
}

/// The procedural database as cooked files: (database file, skeleton id, clip ids, id → bytes).
struct Cooked {
    database: Vec<u8>,
    skeleton_id: AssetId,
    clip_ids: Vec<AssetId>,
    files: Vec<(AssetId, Vec<u8>)>,
}

fn cook() -> Cooked {
    let set = procedural_locomotion();
    let skeleton_id = AssetId::new_random();
    let skeleton = postcard::to_allocvec(&set.skeleton).expect("skeleton");
    let mut files = vec![(
        skeleton_id,
        cooked(skeleton_type_tag(), &skeleton, skeleton_id, vec![]),
    )];
    let mut clip_ids = Vec::new();
    for clip in set.entry_clips() {
        let id = AssetId::new_random();
        let payload = clip.to_cooked_payload().expect("clip");
        files.push((id, cooked(clip_type_tag(), &payload, id, vec![])));
        clip_ids.push(id);
    }
    let payload = set.build().expect("database").to_cooked_payload().expect("cook");
    let mut deps = vec![skeleton_id];
    deps.extend(&clip_ids);
    let database = cooked(motion_database_type_tag(), &payload, AssetId::new_random(), deps);
    Cooked {
        database,
        skeleton_id,
        clip_ids,
        files,
    }
}

impl Cooked {
    fn read(&self, id: AssetId) -> Result<Vec<u8>, String> {
        self.files
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| format!("no file {id}"))
    }
}

#[test]
fn a_cooked_database_loads_with_its_skeleton_and_every_clip() {
    let c = cook();
    assert!(c.clip_ids.len() >= 2, "the fixture needs more than one clip");
    let loaded = load_motion_database_file(&c.database, true, |id| c.read(id)).expect("load");
    assert_eq!(loaded.clips.as_ref().map(|c| c.len()), Some(c.clip_ids.len()));
    assert!(loaded.skeleton.is_some());
    assert!(loaded.player().is_some());
    // Without dependencies the reader is never called and nothing but the database loads.
    let bare = load_motion_database_file(&c.database, false, |_| panic!("no read expected")).expect("bare");
    assert!(bare.skeleton.is_none() && bare.clips.is_none() && bare.player().is_none());
}

#[test]
fn a_missing_clip_names_its_dependency_index() {
    let c = cook();
    let last = c.clip_ids.len() - 1;
    let gone = c.clip_ids[last];
    let err = load_motion_database_file(&c.database, true, |id| {
        if id == gone {
            Err("deleted".into())
        } else {
            c.read(id)
        }
    })
    .expect_err("a clip is missing");
    assert_eq!(err.code(), "missing_dependency");
    match err {
        MotionDatabaseAssetError::Dependency { index, id, reason } => {
            assert_eq!(index, last + 1, "index 0 is the skeleton");
            assert_eq!(id, gone);
            assert_eq!(reason, "deleted");
        }
        other => panic!("wrong error {other:?}"),
    }
}

#[test]
fn a_damaged_skeleton_is_dependency_zero_and_a_wrong_count_is_corrupt() {
    let c = cook();
    let err = load_motion_database_file(&c.database, true, |id| {
        if id == c.skeleton_id {
            Ok(vec![1, 2, 3])
        } else {
            c.read(id)
        }
    })
    .expect_err("damaged skeleton");
    assert!(
        matches!(err, MotionDatabaseAssetError::Dependency { index: 0, .. }),
        "{err:?}"
    );

    // The same database cooked with one dependency too few.
    let set = procedural_locomotion();
    let payload = set.build().expect("db").to_cooked_payload().expect("cook");
    let short: Vec<AssetId> = std::iter::once(c.skeleton_id)
        .chain(c.clip_ids.iter().skip(1).copied())
        .collect();
    let file = cooked(motion_database_type_tag(), &payload, AssetId::new_random(), short);
    let err = load_motion_database_file(&file, true, |id| c.read(id)).expect_err("count");
    assert_eq!(err.code(), "corrupt_database");
}

#[test]
fn a_truncated_or_wrong_type_file_is_corrupt_not_a_panic() {
    let c = cook();
    for len in [0, 3, 40, c.database.len() / 2, c.database.len() - 1] {
        let err =
            load_motion_database_file(&c.database[..len], true, |id| c.read(id)).expect_err("truncated");
        assert_eq!(err.code(), "corrupt_database", "length {len}");
    }
    // A clip file handed over as the database is a type mismatch, reported as corrupt.
    let (_, clip_file) = &c.files[1];
    let err = load_motion_database_file(clip_file, false, |_| Err(String::new())).expect_err("type");
    assert_eq!(err.code(), "corrupt_database");
}

#[test]
fn the_registry_resolves_asset_prefixes_bumps_generations_and_finds_ids() {
    let set = procedural_locomotion();
    let make =
        || LoadedMotionDatabase::built(set.build().expect("db"), set.skeleton.clone(), set.entry_clips());
    let mut registry = MotionDatabases::default();
    assert_eq!(registry.insert("assets/a/walk.motiondb.ron", make()), 1);
    assert!(
        registry.get("a/walk.motiondb.ron").is_some(),
        "without the prefix"
    );
    assert!(registry.get("assets/a/walk.motiondb.ron").is_some());
    assert!(registry.get("a/run.motiondb.ron").is_none());
    assert_eq!(registry.insert("a/run.motiondb.ron", make()), 1);
    assert!(
        registry.get("assets/a/run.motiondb.ron").is_some(),
        "with the prefix"
    );
    assert_eq!(registry.insert("assets/a/walk.motiondb.ron", make()), 2);
    let paths: Vec<&str> = registry.iter().map(|(p, _)| p).collect();
    assert_eq!(
        paths,
        vec!["a/run.motiondb.ron", "assets/a/walk.motiondb.ron"],
        "sorted by path"
    );
    let shared = registry.get("a/run.motiondb.ron").expect("run").database.clone();
    assert_eq!(registry.path_of_database(&shared), Some("a/run.motiondb.ron"));
    let other = std::sync::Arc::new(set.build().expect("db"));
    assert_eq!(
        registry.path_of_database(&other),
        None,
        "a different Arc is not a match"
    );
    // The positive path: a load that recorded its asset id resolves back to its path.
    let db_id = AssetId::new_random();
    let mut identified = make();
    identified.id = Some(db_id);
    assert_eq!(registry.insert("assets/a/idle.motiondb.ron", identified), 1);
    assert_eq!(registry.path_of(db_id), Some("assets/a/idle.motiondb.ron"));
    assert_eq!(
        registry.path_of(AssetId::new_random()),
        None,
        "an unknown id has no path"
    );
    assert_eq!(registry.errors().count(), 0);
}
