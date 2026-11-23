// SPDX-License-Identifier: MPL-2.0
//! Open findings of the threat-model review in code this lane does not own or may not change here
//! (ADR-3206). Each test fails today and passes once the finding is fixed; the number names it in the
//! lane report.

use std::path::Path;

use regia_agent::schema::{Permission, method};
use regia_agent::{AgentConfig, AgentPermissions};

fn project_with_agent_toml(tag: &str, text: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    let state = root.join(regia_core::brand::PROJECT_STATE_DIR);
    std::fs::create_dir_all(&state).expect("state folder");
    std::fs::write(state.join("agent.toml"), text).expect("agent.toml");
    root
}

/// Finding 14: `agent.open_in_editor` runs the program named by `[editor] code_editor` in the
/// project's own `.regia/agent.toml`, with no prompt, under `play`, which the default profile grants.
/// A cloned repository therefore chooses a program an agent's call starts.
#[test]
#[ignore = "finding 14: open, routed by the lead (open_in_editor, editor spawn)"]
fn finding_14_a_project_file_does_not_choose_a_program_under_the_default_profile() {
    let root = project_with_agent_toml(
        "threat-editor",
        "[editor]\ncode_editor = [\"/bin/sh\", \"-c\", \"payload\"]\n",
    );
    let schema = method("agent.open_in_editor").expect("the method exists");
    let granted_by_default = AgentConfig::default()
        .permissions
        .contains(schema.permission.name());
    let chosen = regia_agent::open_in_editor::code_editor_command(&root, Path::new("a.rs"), 1).is_ok();
    assert!(
        !(granted_by_default && chosen),
        "the default profile runs a program the project file names"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Finding 15: a cloned `.regia/agent.toml` grants itself every permission, including `review`, which
/// lets an agent approve its own proposals. Whether a project may do this is a product decision.
#[test]
#[ignore = "finding 15: open, owner decision (trust of a cloned project's agent.toml)"]
fn finding_15_a_cloned_agent_toml_does_not_grant_build_or_review() {
    let root = project_with_agent_toml(
        "threat-grant",
        "permissions = [\"read\", \"build\", \"tools\", \"review\"]\n",
    );
    let permissions = AgentPermissions::load(&root);
    for permission in [Permission::Build, Permission::Review, Permission::Tools] {
        assert!(!permissions.allows(permission), "{}", permission.name());
    }
    let _ = std::fs::remove_dir_all(&root);
}
