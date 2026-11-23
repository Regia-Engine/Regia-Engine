// SPDX-License-Identifier: MIT-0
//! Feature anchor for hot reload (`docs/specs/20` § 1).
//!
//! The editor you run is the engine's `regia-editor --project <root>`. This crate exists
//! so a rebuild of `game` sees the same engine features as that editor. It links the game
//! crate and does not duplicate the editor UI.

fn main() {
    // Keep the game crate linked. Running this bin is not how you open the project.
    let _anchor = std::any::type_name::<{{crate_name}}::Game>();
    eprintln!("{{name}}: open this project with `regia-editor --project .`");
}
