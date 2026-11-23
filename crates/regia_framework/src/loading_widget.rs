// SPDX-License-Identifier: MPL-2.0
//! Pipeline compilation feedback built from the renderer's actual work queue (ADR-8051).
//!
//! Add [`PipelineLoadingPlugin`], then call [`spawn_pipeline_loading_widget`] under a UI root.
//! Hosts with their own renderer loop can instead call [`apply_pipeline_warm_progress`] with
//! a snapshot and a monotonic timestamp. No graphics calls or background workers live here.

use regia_app::prelude::*;
use regia_render::{PipelineWarmProgress, PipelineWarmQueue};
use regia_ui::ProgressBar;
use regia_ui::node::{NodeName, NodeStyle, UiNode};
use regia_ui::style::{Dimension, Display, FlexDirection, Style};
use regia_ui::text::Text;
use serde::Serialize;

/// Presentation of the warm queue. Counts are jobs, not an estimate of pipeline objects.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PipelineLoadingState {
    done: u32,
    total: u32,
    deferred_pending: usize,
    fraction: f32,
    busy: bool,
    label: String,
    error: Option<String>,
}

impl PipelineLoadingState {
    /// Refreshes from one coherent renderer snapshot, including deferred work and errors.
    pub fn update(&mut self, progress: &PipelineWarmProgress) {
        self.done = progress.done.min(progress.total);
        self.total = progress.total;
        self.deferred_pending = progress.deferred_pending;
        self.fraction = progress.fraction().clamp(0.0, 1.0);
        self.busy = self.done < self.total || self.deferred_pending > 0;
        self.error.clone_from(&progress.last_error);
        self.label = if let Some(error) = &self.error {
            format!("Shader preparation failed: {error}")
        } else if self.busy {
            let mut line = format!("Preparing shaders: {} / {} jobs", self.done, self.total);
            if self.deferred_pending > 0 {
                line.push_str(&format!(", {} deferred", self.deferred_pending));
            }
            line
        } else {
            "Shaders ready".into()
        };
    }

    /// Fraction of queued jobs finished, clamped to `0..=1`; empty queues are complete.
    /// Deferred jobs remain a separate count because their total is not known here.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        self.fraction
    }

    /// Whether queued or deferred jobs remain, even before the worker starts.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Human-readable status, shared with the displayed label and agent inspection.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Last compile error; completion must not silently erase it.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Delay policy for shader notices. All values are milliseconds of continuous pending work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ShaderNoticeSettings {
    /// Delay on a cached or unknown-cache launch. The notice appears strictly after it.
    pub delay_ms: u64,
    /// Delay when the cache reports missing or discarded data.
    pub uncached_delay_ms: u64,
}

impl Default for ShaderNoticeSettings {
    fn default() -> Self {
        Self {
            delay_ms: 5_000,
            uncached_delay_ms: 1_500,
        }
    }
}

/// Delayed, nonblocking shader status. Explicit timestamps make tests and replay deterministic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ShaderCompileNotice {
    #[serde(skip)]
    busy_since_ms: Option<u64>,
    visible: bool,
    line: String,
}

impl ShaderCompileNotice {
    /// Refreshes the notice. Returns whether its visible output changed. Completion hides it
    /// immediately and resets the delay for the next warm. Clock regressions restart the wait.
    pub fn update(
        &mut self,
        progress: &PipelineWarmProgress,
        now_ms: u64,
        settings: ShaderNoticeSettings,
    ) -> bool {
        let before = (self.visible, self.line.clone());
        let busy = progress.done < progress.total || progress.deferred_pending > 0;
        if !busy || progress.last_error.is_some() {
            self.busy_since_ms = None;
            self.visible = false;
        } else {
            let since = self.busy_since_ms.get_or_insert(now_ms);
            if now_ms < *since {
                *since = now_ms;
            }
            let uncached = matches!(
                progress.cache.load,
                Some(
                    regia_render::pipeline_cache::CacheLoad::Missing
                        | regia_render::pipeline_cache::CacheLoad::Discarded(_)
                )
            );
            let delay = if uncached {
                settings.uncached_delay_ms
            } else {
                settings.delay_ms
            };
            self.visible = now_ms.saturating_sub(*since) > delay;
        }
        self.line = if self.visible {
            let mut state = PipelineLoadingState::default();
            state.update(progress);
            state.label
        } else {
            String::new()
        };
        before != (self.visible, self.line.clone())
    }

    /// Whether the delayed notice is visible.
    #[must_use]
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The line shown, empty while hidden; no guessed totals or completion-time promises.
    #[must_use]
    pub fn line(&self) -> &str {
        &self.line
    }
}

/// A loading panel's entities and current presentation state. Despawning its root removes
/// its children through the normal UI hierarchy. Several panels may observe the same queue.
#[derive(Component, Clone, Debug)]
pub struct PipelineLoadingWidget {
    /// Status text child.
    pub label: Entity,
    /// Progress bar child (queue fraction; deferred work is counted in the label).
    pub bar: Entity,
    /// Delayed compile notice child.
    pub notice: Entity,
    /// Notice delay policy, adjustable at runtime.
    pub notice_settings: ShaderNoticeSettings,
    /// Hide the panel when ready. Errors stay visible. Default `true`.
    pub hide_when_ready: bool,
    /// Show the progress label and bar immediately. When `false`, only the delayed notice
    /// is shown (errors still appear immediately). Default `true`.
    pub show_progress: bool,
    state: PipelineLoadingState,
    shader_notice: ShaderCompileNotice,
}

impl PipelineLoadingWidget {
    /// Last snapshot applied to the visible children.
    #[must_use]
    pub fn state(&self) -> &PipelineLoadingState {
        &self.state
    }

    /// Last delayed notice applied to the notice child.
    #[must_use]
    pub fn shader_notice(&self) -> &ShaderCompileNotice {
        &self.shader_notice
    }
}

/// Creates a themeable panel under `parent`, which must belong to a UI root. The caller owns
/// its position and size. It is hidden until a progress snapshot arrives, takes no focus or
/// input, and uses the default font. Returns the panel entity carrying [`PipelineLoadingWidget`].
pub fn spawn_pipeline_loading_widget(world: &mut World, parent: Entity) -> Entity {
    let root = world
        .spawn((
            UiNode,
            NodeName("pipeline_loading".into()),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                display: Display::None,
                ..Default::default()
            }),
        ))
        .id();
    let label = world
        .spawn((
            UiNode,
            NodeName("pipeline_loading_label".into()),
            NodeStyle::default(),
            Text::new("Preparing shaders"),
        ))
        .id();
    let bar = world
        .spawn((
            UiNode,
            NodeName("pipeline_loading_bar".into()),
            ProgressBar::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(4.0)],
                ..Default::default()
            }),
        ))
        .id();
    let notice = world
        .spawn((
            UiNode,
            NodeName("shader_compile_notice".into()),
            Text::new(""),
            NodeStyle::new(Style {
                display: Display::None,
                ..Default::default()
            }),
        ))
        .id();
    world
        .entity_mut(root)
        .add_children(&[label, bar, notice])
        .insert(PipelineLoadingWidget {
            label,
            bar,
            notice,
            notice_settings: ShaderNoticeSettings::default(),
            hide_when_ready: true,
            show_progress: true,
            state: PipelineLoadingState::default(),
            shader_notice: ShaderCompileNotice::default(),
        });
    world.entity_mut(parent).add_child(root);
    root
}

/// Updates existing panels from the renderer snapshot. No writes on unchanged frames.
/// This is also the adapter for hosts that do not use [`PipelineLoadingPlugin`].
pub fn apply_pipeline_warm_progress(world: &mut World, progress: &PipelineWarmProgress, now_ms: u64) {
    let _span = tracing::trace_span!("pipeline_loading_widgets").entered();
    let mut state = PipelineLoadingState::default();
    state.update(progress);
    let mut widgets = world.query::<(Entity, &mut PipelineLoadingWidget)>();
    let mut updates = Vec::new();
    for (root, mut widget) in widgets.iter_mut(world) {
        let mut notice = widget.shader_notice.clone();
        let notice_changed = notice.update(progress, now_ms, widget.notice_settings);
        if widget.state != state {
            widget.state = state.clone();
        }
        // The timer is private but must advance even before the notice becomes visible.
        if widget.shader_notice != notice {
            widget.shader_notice = notice;
        }
        updates.push((
            root,
            widget.label,
            widget.bar,
            widget.notice,
            !widget.hide_when_ready
                || state.error.is_some()
                || (state.busy && (widget.show_progress || widget.shader_notice.visible())),
            widget.show_progress || state.error.is_some(),
            widget.show_progress,
            widget.shader_notice.visible(),
            notice_changed,
            widget.shader_notice.line().to_owned(),
        ));
    }
    for (
        root,
        label,
        bar,
        notice,
        visible,
        label_visible,
        bar_visible,
        notice_visible,
        notice_changed,
        line,
    ) in updates
    {
        set_display(world, root, visible);
        set_display(world, label, label_visible);
        set_display(world, bar, bar_visible);
        set_text(world, label, state.label());
        if let Some(mut value) = world.get_mut::<ProgressBar>(bar)
            && value.value != state.fraction
        {
            value.value = state.fraction;
        }
        set_display(world, notice, notice_visible);
        if notice_changed {
            set_text(world, notice, &line);
        }
    }
}

fn set_display(world: &mut World, entity: Entity, visible: bool) {
    let display = if visible { Display::Flex } else { Display::None };
    if let Some(mut style) = world.get_mut::<NodeStyle>(entity)
        && style.0.display != display
    {
        style.0.display = display;
    }
}

fn set_text(world: &mut World, entity: Entity, line: &str) {
    let Some(mut text) = world.get_mut::<Text>(entity) else {
        return;
    };
    if text.spans.first().is_some_and(|span| {
        matches!(&span.content,
        regia_ui::text::TextContent::Lit(current) if current == line)
    }) {
        return;
    }
    if let Some(span) = text.spans.first_mut() {
        span.content = line.into();
    } else {
        text.spans.push(regia_ui::text::TextSpan::lit(line));
    }
}

/// Feeds panels before UI layout; no queue or no panels means no work. Install alongside
/// `UiPlugin` and the renderer, or use the explicit snapshot adapter for a custom host.
#[derive(Debug, Default)]
pub struct PipelineLoadingPlugin;

impl Plugin for PipelineLoadingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            update_pipeline_loading_widgets.before(PostUpdateSet::UiLayout),
        );
    }
}

fn update_pipeline_loading_widgets(world: &mut World) {
    if world
        .query::<&PipelineLoadingWidget>()
        .iter(world)
        .next()
        .is_none()
    {
        return;
    }
    let Some(queue) = world.get_resource::<PipelineWarmQueue>() else {
        return;
    };
    let progress = queue.progress();
    let now_ms = world.get_resource::<regia_app::RealTime>().map_or(0, |time| {
        time.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    });
    apply_pipeline_warm_progress(world, &progress, now_ms);
}

/// Creates a standalone delayed shader notice under a host's UI root. It uses the same
/// snapshot adapter and plugin as the full loading widget, but hides its progress label
/// and bar. Errors show immediately. The returned panel carries [`PipelineLoadingWidget`].
pub fn spawn_shader_compile_notice(world: &mut World, parent: Entity) -> Entity {
    let root = spawn_pipeline_loading_widget(world, parent);
    if let Some(mut widget) = world.get_mut::<PipelineLoadingWidget>(root) {
        widget.show_progress = false;
    }
    root
}
