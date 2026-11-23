// SPDX-License-Identifier: MPL-2.0
//! HLOD voxel proxies of the terrain scatter and the proxy budget at run time (ADR-3253, spec 78
//! §§ 11.3, 11.6). Lockstep, fixed steps, residency marked by the test (no renderer).

mod common;

use common::*;
use regia_scene::hlod::{CellProxy, HlodConfig, HlodTree};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellMember, CellRenderHidden, HlodCut, HlodProxyMesh, HlodProxyResidency, HlodProxyVoxels, HlodSettings,
    HlodVoxelResidency,
};
use regia_world::WorldStreaming;
use regia_world::format::{HlodIndex, hlod_proxy_id};
use regia_world::hlod_voxels::{HLOD_VOXELS_FORMAT_VERSION, HlodVoxelNode, HlodVoxels, hlod_voxel_proxy_id};

fn c(x: i64, z: i64) -> CellCoord {
    CellCoord::new(x, z)
}

/// An HLOD index over `cells` with the cook's derived threshold for `load` (leaves switch to full
/// detail inside the load radius), every node with a mesh proxy unless `no_mesh` names its cell.
fn hlod_over(world: &World, cells: &[CellCoord], load: f64, no_mesh: &[CellCoord]) -> (HlodIndex, HlodTree) {
    let grid = world.index.settings.grid().expect("grid");
    let proxies: Vec<CellProxy> = cells
        .iter()
        .map(|cell| CellProxy {
            cell: *cell,
            min_y: 0.0,
            max_y: 10.0,
            bytes: 1000,
        })
        .collect();
    let probe = HlodTree::build(&grid, HlodConfig::default(), &proxies).expect("probe");
    let mut radii: Vec<f64> = probe
        .nodes()
        .iter()
        .filter(|n| n.level == 0)
        .map(|n| n.radius)
        .collect();
    radii.sort_by(f64::total_cmp);
    let config = HlodSettings::default().config(load, radii[radii.len() / 2]);
    let tree = HlodTree::build(&grid, config, &proxies).expect("tree");
    let nodes = tree.nodes().len();
    let node_proxies = (0..nodes)
        .map(|n| {
            let cell = tree.node(n as u32).and_then(|node| node.cell);
            (!cell.is_some_and(|cell| no_mesh.contains(&cell))).then(|| hlod_proxy_id(level(), n as u32))
        })
        .collect();
    let index = HlodIndex {
        config,
        proxies,
        node_proxies,
        node_anchors: vec![[0; 3]; nodes],
        node_colors: vec![None; nodes],
        node_atlases: Vec::new(),
    };
    (index, tree)
}

fn hidden_members(app: &mut regia_app::App, cell: CellCoord) -> usize {
    app.world_mut()
        .query_filtered::<&CellMember, regia_ecs::prelude::With<CellRenderHidden>>()
        .iter(app.world())
        .filter(|m| m.0 == cell)
        .count()
}

/// What a renderer does for the runtime: every spawned proxy entity loads and becomes resident (a
/// node the budget did not keep has no entity, so it never becomes resident).
fn load_spawned(app: &mut regia_app::App) {
    let meshes: Vec<u32> = app
        .world_mut()
        .query::<&HlodProxyMesh>()
        .iter(app.world())
        .map(|p| p.node)
        .collect();
    let voxels: Vec<u32> = app
        .world_mut()
        .query::<&HlodProxyVoxels>()
        .iter(app.world())
        .map(|p| p.node)
        .collect();
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(meshes);
    app.world_mut()
        .resource_mut::<HlodVoxelResidency>()
        .resident
        .extend(voxels);
}

fn mark_all_resident(app: &mut regia_app::App, nodes: usize) {
    app.world_mut()
        .resource_mut::<HlodProxyResidency>()
        .resident
        .extend(0..nodes as u32);
    app.world_mut()
        .resource_mut::<HlodVoxelResidency>()
        .resident
        .extend(0..nodes as u32);
}

/// Spec 78 § 11.3 as amended by ADR-3253: a zero or tiny proxy budget means no prefetch, never a
/// root proxy standing in for the cell the viewer stands in. Before, the budget evicted every
/// non-root proxy the cut drew, the repair raised the cut to the root, and the root hid the
/// viewer's own (loaded, visible) cell.
#[test]
fn a_zero_or_tiny_proxy_budget_still_draws_the_viewers_cell_in_full() {
    for budget in [0u64, 1] {
        let mut app = app();
        let mut s = settings(150.0, 200.0);
        if let Some(b) = s.budgets.as_mut() {
            b.hlod_budget_bytes = budget;
        }
        let mut world = build(&app, 4, 1, 2, 1, s);
        let cells: Vec<CellCoord> = (0..4).map(|x| c(x, 0)).collect();
        let (index, tree) = hlod_over(&world, &cells, 150.0, &[]);
        world.index.hlod = Some(index);
        open(&mut app, &world, 1);
        assert!(tree.nodes().len() > 4);
        spawn_source(&mut app, 50.0, 50.0);
        // Residency comes only from proxies that spawned and loaded.
        for _ in 0..12 {
            app.update();
            load_spawned(&mut app);
        }
        assert_eq!(
            state(&app, c(0, 0)),
            Some(regia_world::CellState::Visible),
            "budget {budget}"
        );
        let cut = app.world().resource::<HlodCut>().clone();
        assert!(
            cut.entries.iter().any(|e| !e.proxy && e.cell == Some(c(0, 0))),
            "budget {budget}: the viewer's cell draws in full: {cut:?}"
        );
        assert_eq!(hidden_members(&mut app, c(0, 0)), 0, "budget {budget}");
        assert!(
            cut.entries.iter().any(|e| e.proxy),
            "budget {budget}: the far cells draw as proxies"
        );
        assert_eq!(stats(&app).holes, 0, "budget {budget}");
    }
}

/// A large budget keeps what it kept before: the drawn proxies plus their parents.
#[test]
fn a_large_budget_still_prefetches_parents() {
    let mut app = app();
    let world = {
        let mut world = build(&app, 4, 1, 1, 1, settings(150.0, 200.0));
        let cells: Vec<CellCoord> = (0..4).map(|x| c(x, 0)).collect();
        world.index.hlod = Some(hlod_over(&world, &cells, 150.0, &[]).0);
        world
    };
    let tree = world
        .index
        .hlod
        .as_ref()
        .expect("hlod")
        .tree(&world.index.settings.grid().expect("grid"))
        .expect("tree");
    open(&mut app, &world, 1);
    mark_all_resident(&mut app, tree.nodes().len());
    spawn_source(&mut app, 50.0, 50.0);
    steps(&mut app, 10);
    let spawned: Vec<u32> = app
        .world_mut()
        .query::<&HlodProxyMesh>()
        .iter(app.world())
        .map(|p| p.node)
        .collect();
    let cut = app.world().resource::<HlodCut>().clone();
    let drawn = cut.entries.iter().filter(|e| e.proxy).count();
    assert!(
        spawned.len() > drawn,
        "parents are held beside the drawn proxies: {spawned:?} vs {cut:?}"
    );
}

/// A leaf whose cell has no record (scatter only, ADR-3253) draws in full near the viewer: its full
/// detail is the terrain's own scatter, so no proxy stands in for it there; far away its voxel
/// proxy draws, and a node with a mesh and a voxel proxy draws once both are resident.
#[test]
fn a_scatter_only_leaf_draws_its_voxels_far_and_nothing_near() {
    let mut app = app();
    let mut world = build(&app, 2, 1, 1, 1, settings(150.0, 200.0));
    world.index.bounds = [c(0, 0), c(2, 0)];
    let cells = [c(0, 0), c(1, 0), c(2, 0)];
    let (index, tree) = hlod_over(&world, &cells, 150.0, &[c(2, 0)]);
    let nodes = tree.nodes().len();
    let forest = tree.leaf_for(c(2, 0)).expect("leaf");
    let root = tree.roots()[0];
    world.index.hlod = Some(index);
    open(&mut app, &world, 1);
    let voxel = |node: u32| HlodVoxelNode {
        proxy: hlod_voxel_proxy_id(level(), node),
        anchor: [0; 3],
        instances: 50,
        voxels: 400,
        voxel_size_m: 3.0,
        bytes: 5000,
    };
    let mut table = HlodVoxels {
        format_version: HLOD_VOXELS_FORMAT_VERSION,
        level: level().0,
        resolution: 32,
        nodes: vec![None; nodes],
    };
    table.nodes[forest as usize] = Some(voxel(forest));
    table.nodes[root as usize] = Some(voxel(root));
    {
        let mut streaming = app.world_mut().resource_mut::<WorldStreaming>();
        // Refused: another level's table, or one longer than the tree.
        let mut wrong = table.clone();
        wrong.level = regia_core::id::Guid::from_u128(1);
        assert!(streaming.set_hlod_voxels(level(), wrong).is_err());
        let mut long = table.clone();
        long.nodes.resize(nodes + 1, None);
        assert!(streaming.set_hlod_voxels(level(), long).is_err());
        streaming.set_hlod_voxels(level(), table).expect("table");
        assert_eq!(streaming.hlod_voxel_nodes(), 2);
    }
    mark_all_resident(&mut app, nodes);

    // In the forest cell: it draws in full (the scatter), not as a proxy, and nothing is a hole.
    let source = spawn_source(&mut app, 250.0, 50.0);
    steps(&mut app, 10);
    let cut = app.world().resource::<HlodCut>().clone();
    let entry = cut
        .entries
        .iter()
        .find(|e| e.node == forest)
        .expect("the forest leaf is in the cut");
    assert!(!entry.proxy, "near, the forest is the scatter itself: {cut:?}");
    assert_eq!(stats(&app).holes, 0);
    let drawn_voxels = |app: &mut regia_app::App| -> Vec<u32> {
        let mut nodes: Vec<u32> = app
            .world_mut()
            .query_filtered::<&HlodProxyVoxels, regia_ecs::prelude::Without<CellRenderHidden>>()
            .iter(app.world())
            .map(|v| v.node)
            .collect();
        nodes.sort_unstable();
        nodes
    };
    assert!(
        !drawn_voxels(&mut app).contains(&forest),
        "no voxel proxy over the viewer's forest"
    );

    // Far away: the root draws, with its mesh and its voxels.
    move_to(&mut app, source, 3000.0, 50.0);
    steps(&mut app, 30);
    mark_all_resident(&mut app, nodes);
    steps(&mut app, 2);
    let cut = app.world().resource::<HlodCut>().clone();
    assert_eq!(
        cut.entries.iter().map(|e| (e.node, e.proxy)).collect::<Vec<_>>(),
        vec![(root, true)]
    );
    assert_eq!(drawn_voxels(&mut app), vec![root]);
    let voxel_entity = app
        .world_mut()
        .query::<&HlodProxyVoxels>()
        .iter(app.world())
        .find(|v| v.node == root)
        .copied()
        .expect("root voxels");
    assert_eq!(voxel_entity.proxy, hlod_voxel_proxy_id(level(), root).0);

    // Without its voxels resident the root is not ready: the cut goes to its children.
    app.world_mut()
        .resource_mut::<HlodVoxelResidency>()
        .resident
        .remove(&root);
    steps(&mut app, 2);
    let cut = app.world().resource::<HlodCut>().clone();
    assert!(cut.entries.iter().all(|e| e.node != root), "{cut:?}");
}
