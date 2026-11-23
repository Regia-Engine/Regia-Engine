# Profiler

Two floating windows, turned on and off from the **View** menu. **View ▸ Profiler** shows the
frame time, a flame view of the span call tree and the GPU passes, and captures a frame.
**View ▸ Diagnostics** has a **GPU** tab (GPU breadcrumbs and the last crash bundle) and a
**Timeline** tab that records and shows a `.regiatrace`. Depth: [Profiler
(agents)](../../agents/profiler.md), [Editor diagnostics](../../agents/editor-diagnostics.md),
[Profiling and performance tools](../../systems/profiling.md), [Performance
overlay](../performance-overlay.md).

## Profiler window

| Action | Control | Agent method |
|---|---|---|
| Show or hide | **View ▸ Profiler** | none (the window is UI only; the data is below) |
| Record the call tree | **Record call tree** | `profiler.set_capture` |
| Capture a frame to a file | **Capture** | `profiler.capture` |
| Read a frame's spans | The flame view (hover a span for its name and time) | `profiler.frame` |

## Diagnostics window

| Action | Control | Agent method |
|---|---|---|
| Show or hide | **View ▸ Diagnostics** | none |
| GPU breadcrumbs | **GPU** tab | `editor.diagnostics` |
| Load the last crash bundle | **Load last crash bundle** | `editor.diagnostics` with `load_crash` |
| Record a trace | **Timeline** tab ▸ **Record**, **Stop recording** | `editor.trace_record` |
| Open the newest trace | **Open newest** | `editor.trace_timeline` |
| Zoom and pan the timeline | **−**, **+**, the **pan** slider | `editor.trace_timeline` |

## Keyboard and accessibility notes

The flame view and the trace timeline are drawn shapes, announced as "Flame view, N spans" and
"Trace timeline, N items in view". Under each is a focusable list (ADR-7383) with what a hover
shows: **Spans** rows read `<name> in <parent>, main thread, <inclusive> ms inclusive, <self> ms
self, <calls> calls` (every span of the call tree, the ones too thin for a bar too), and **Items in
view** rows read `<name>, <lane> lane, frame <n>, <ms> ms` (marks and screenshots name their frame).
Tab reaches the list; Up and Down move through it. The agent twins are `profiler.frame` and
`editor.trace_timeline`. Findings: [Editor accessibility](accessibility.md).

A dockable profiler panel with frame history and counters is on the branch
`lane/sprint-v06-profiler-panel`; this page gains it at the v0.6 integration.
