// SPDX-License-Identifier: MPL-2.0
//! Pipeline warm UI acceptance without a graphics device.

use regia_framework::loading_widget::PipelineLoadingState;
use regia_render::PipelineWarmProgress;

#[test]
fn loading_widget_tracks_warm_progress() {
    let mut state = PipelineLoadingState::default();
    let mut progress = PipelineWarmProgress {
        started: true,
        done: 2,
        total: 8,
        ..Default::default()
    };
    state.update(&progress);
    assert_eq!(state.fraction(), 0.25);
    assert!(state.busy());
    assert!(state.label().contains("2 / 8"));
    progress.done = 8;
    state.update(&progress);
    assert_eq!(state.fraction(), 1.0);
    assert!(!state.busy());
    progress.deferred_pending = 3;
    state.update(&progress);
    assert!(state.busy(), "deferred compilation keeps the widget visible");
    assert!(state.label().contains("3 deferred"));
    progress.deferred_pending = 0;
    progress.last_error = Some("compile failed".into());
    state.update(&progress);
    assert_eq!(state.error(), Some("compile failed"));
    assert!(!state.busy());
}

#[test]
fn loading_widget_empty_overfull_and_restarted_queues() {
    let mut state = PipelineLoadingState::default();
    let mut progress = PipelineWarmProgress::default();
    state.update(&progress);
    assert_eq!(state.fraction(), 1.0);
    assert!(!state.busy());
    progress.total = 4;
    progress.done = 9;
    state.update(&progress);
    assert_eq!(state.fraction(), 1.0);
    assert!(!state.busy());
    progress.done = 0;
    state.update(&progress);
    assert_eq!(state.fraction(), 0.0);
    assert!(state.busy(), "queued work counts before workers start");
}

#[test]
fn shader_notice_delays_resets_and_uses_cache_status() {
    use regia_framework::loading_widget::{ShaderCompileNotice, ShaderNoticeSettings};
    use regia_render::pipeline_cache::CacheLoad;
    let mut notice = ShaderCompileNotice::default();
    let settings = ShaderNoticeSettings {
        delay_ms: 100,
        uncached_delay_ms: 20,
    };
    let mut progress = PipelineWarmProgress {
        total: 4,
        ..Default::default()
    };
    assert!(!notice.update(&progress, 1_000, settings));
    assert!(!notice.update(&progress, 1_100, settings));
    assert!(notice.update(&progress, 1_101, settings));
    assert!(notice.visible());
    assert!(!notice.update(&progress, 1_200, settings));
    progress.done = 4;
    assert!(notice.update(&progress, 1_201, settings));
    assert!(!notice.visible());
    progress.deferred_pending = 2;
    progress.cache.load = Some(CacheLoad::Missing);
    assert!(!notice.update(&progress, 2_000, settings));
    assert!(!notice.update(&progress, 2_020, settings));
    assert!(notice.update(&progress, 2_021, settings));
    assert!(notice.line().contains("2 deferred"));
    assert!(
        notice.update(&progress, 500, settings),
        "clock regression hides/restarts"
    );
    assert!(!notice.visible());
    assert!(notice.update(&progress, 521, settings));
    progress.last_error = Some("compile failed".into());
    assert!(notice.update(&progress, 522, settings));
    assert!(!notice.visible(), "the error label replaces the compile notice");
    progress.last_error = None;
    progress.deferred_pending = 0;
    assert!(!notice.update(&progress, 5_000, settings));
}

#[test]
fn loading_widget_updates_real_entities_and_retains_errors() {
    use regia_app::prelude::*;
    use regia_framework::loading_widget::{
        PipelineLoadingWidget, apply_pipeline_warm_progress, spawn_pipeline_loading_widget,
    };
    use regia_ui::style::Display;
    use regia_ui::{NodeStyle, ProgressBar, Text, UiNode, UiRoot};
    let mut world = World::new();
    let parent = world.spawn((UiRoot::default(), UiNode)).id();
    let panel = spawn_pipeline_loading_widget(&mut world, parent);
    let widget = world.get::<PipelineLoadingWidget>(panel).expect("widget").clone();
    let mut progress = PipelineWarmProgress {
        total: 8,
        done: 2,
        started: true,
        ..Default::default()
    };
    apply_pipeline_warm_progress(&mut world, &progress, 0);
    assert_eq!(world.get::<ProgressBar>(widget.bar).expect("bar").value, 0.25);
    assert_eq!(
        world.get::<NodeStyle>(panel).expect("panel").0.display,
        Display::Flex
    );
    assert_eq!(
        *world.get::<Text>(widget.label).expect("text"),
        Text::new("Preparing shaders: 2 / 8 jobs")
    );
    apply_pipeline_warm_progress(&mut world, &progress, 5_001);
    assert_eq!(
        world.get::<NodeStyle>(widget.notice).expect("notice").0.display,
        Display::Flex
    );
    progress.done = 8;
    apply_pipeline_warm_progress(&mut world, &progress, 5_002);
    assert_eq!(
        world.get::<NodeStyle>(panel).expect("panel").0.display,
        Display::None
    );
    progress.last_error = Some("compile failed".into());
    apply_pipeline_warm_progress(&mut world, &progress, 5_003);
    assert_eq!(
        world.get::<NodeStyle>(panel).expect("panel").0.display,
        Display::Flex
    );
    assert_eq!(
        *world.get::<Text>(widget.label).expect("error label"),
        Text::new("Shader preparation failed: compile failed")
    );
    world.despawn(panel);
    assert!(world.get_entity(widget.label).is_err());
    apply_pipeline_warm_progress(&mut world, &progress, 5_004);
}

#[test]
fn loading_plugin_reads_the_real_queue_without_starting_a_device() {
    use regia_app::prelude::*;
    use regia_framework::loading_widget::{
        PipelineLoadingPlugin, PipelineLoadingWidget, spawn_pipeline_loading_widget,
    };
    use regia_render::{PipelineWarmQueue, WarmTier};
    let mut app = App::new();
    let queue = PipelineWarmQueue::new(1);
    queue.push(WarmTier::Boot, "test queued work", |_| {
        panic!("no device may run this")
    });
    app.insert_resource(queue).add_plugins(PipelineLoadingPlugin);
    let parent = app.world_mut().spawn_empty().id();
    let panel = spawn_pipeline_loading_widget(app.world_mut(), parent);
    app.update();
    let widget = app.world().get::<PipelineLoadingWidget>(panel).expect("widget");
    assert!(widget.state().busy());
    assert_eq!(widget.state().fraction(), 0.0);
    assert!(widget.state().label().contains("0 / 1"));
    app.world_mut().insert_resource(PipelineWarmQueue::new(1));
    app.update();
    assert!(
        !app.world()
            .get::<PipelineLoadingWidget>(panel)
            .expect("widget")
            .state()
            .busy()
    );
}

#[test]
fn standalone_shader_notice_never_flashes_for_a_fast_warm() {
    use regia_app::prelude::*;
    use regia_framework::loading_widget::{
        PipelineLoadingWidget, apply_pipeline_warm_progress, spawn_shader_compile_notice,
    };
    use regia_ui::style::Display;
    use regia_ui::{NodeStyle, UiNode, UiRoot};
    let mut world = World::new();
    let parent = world.spawn((UiRoot::default(), UiNode)).id();
    let panel = spawn_shader_compile_notice(&mut world, parent);
    let widget = world.get::<PipelineLoadingWidget>(panel).expect("widget").clone();
    let mut progress = PipelineWarmProgress {
        total: 1,
        ..Default::default()
    };
    apply_pipeline_warm_progress(&mut world, &progress, 0);
    assert_eq!(
        world.get::<NodeStyle>(panel).expect("panel").0.display,
        Display::None
    );
    progress.done = 1;
    apply_pipeline_warm_progress(&mut world, &progress, 100);
    assert_eq!(
        world.get::<NodeStyle>(panel).expect("panel").0.display,
        Display::None
    );
    progress.done = 0;
    apply_pipeline_warm_progress(&mut world, &progress, 1_000);
    apply_pipeline_warm_progress(&mut world, &progress, 6_001);
    assert_eq!(
        world.get::<NodeStyle>(panel).expect("panel").0.display,
        Display::Flex
    );
    assert_eq!(
        world.get::<NodeStyle>(widget.label).expect("label").0.display,
        Display::None
    );
    assert_eq!(
        world.get::<NodeStyle>(widget.bar).expect("bar").0.display,
        Display::None
    );
    progress.last_error = Some("compile failed".into());
    apply_pipeline_warm_progress(&mut world, &progress, 6_002);
    assert_eq!(
        world.get::<NodeStyle>(widget.label).expect("error").0.display,
        Display::Flex
    );
    assert_eq!(
        world.get::<NodeStyle>(widget.notice).expect("notice").0.display,
        Display::None
    );
}

#[test]
fn progress_updates_preserve_host_text_styles() {
    use regia_app::prelude::*;
    use regia_framework::loading_widget::{
        PipelineLoadingWidget, apply_pipeline_warm_progress, spawn_pipeline_loading_widget,
    };
    use regia_ui::Text;
    let mut world = World::new();
    let parent = world.spawn_empty().id();
    let panel = spawn_pipeline_loading_widget(&mut world, parent);
    let label = world.get::<PipelineLoadingWidget>(panel).expect("widget").label;
    world.get_mut::<Text>(label).expect("label").spans[0].style.bold = true;
    let progress = PipelineWarmProgress {
        total: 4,
        done: 1,
        ..Default::default()
    };
    apply_pipeline_warm_progress(&mut world, &progress, 0);
    assert!(world.get::<Text>(label).expect("label").spans[0].style.bold);
    assert_eq!(
        world.get::<Text>(label).expect("label").spans[0].content,
        "Preparing shaders: 1 / 4 jobs".into()
    );
}
