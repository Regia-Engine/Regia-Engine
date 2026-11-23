// SPDX-License-Identifier: MIT-0
//! The locomotion graph authored in the editor (`assets/locomotion.animgraph.ron`, produced by
//! `crates/regia_editor/tests/anim_editor.rs` through `editor.anim_*` agent calls) is the
//! graph this sample builds in code, and it loads through the runtime's frozen-format loader.

use regia_animation::graph::AnimGraph;
use regia_animation::params::ParamValue;

#[test]
fn editor_authored_graph_matches_the_code_graph() {
    let text = include_str!("../assets/locomotion.animgraph.ron");
    let authored = AnimGraph::from_ron_str(text).expect("the editor-saved graph loads");
    let code = third_person::anim::locomotion_graph();
    assert_eq!(
        authored.root, code.root,
        "same nodes, states, transitions and IK wrappers"
    );
    // The editor declares every parameter the graph reads (the code graph skips validation).
    let declared: Vec<&str> = authored.parameters.iter().map(|p| p.0.as_str()).collect();
    for name in ["Speed", "Grounded", "Foot0Target", "Foot3Pole", "Foot2Weight"] {
        assert!(declared.contains(&name), "{name} declared: {declared:?}");
    }
    assert!(
        authored
            .parameters
            .iter()
            .any(|p| p.0 == "Grounded" && p.1 == ParamValue::Bool(true)),
        "Grounded defaults to true"
    );
    authored.validate().expect("valid");
}
