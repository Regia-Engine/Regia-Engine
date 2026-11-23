// SPDX-License-Identifier: MPL-2.0
//! `App::new` sizes the global task pools with Regia Engine's formula, so a first `App::update`
//! never leaves the compute pool at bevy's default (every hardware thread).
//!
//! One test per file: the pools are process-global and can only be created once.

use regia_app::{App, Update};
use regia_core::tasks::{self, TaskPoolOptions};

#[test]
fn app_new_applies_the_task_pool_formula_before_the_first_update() {
    assert!(
        tasks::sizes().is_none(),
        "nothing in this process may create the pools first"
    );
    let mut app = App::new();
    app.add_systems(Update, || {});
    app.update();

    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let expected = TaskPoolOptions::default().sizes_for(cpus);
    assert_eq!(tasks::sizes(), Some(expected));
}
