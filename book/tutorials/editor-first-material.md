# Tutorial: your first material graph

This tutorial builds a material in the editor's **Material Graph** panel: a tint parameter
multiplied by the mesh's vertex color, wired to the surface's base color, compiled, undone and
redone, and saved as a `.matgraph.ron`. Each step gives the mouse-and-keyboard way and the agent
call that does the same thing.

The page is a test. `cargo test -p regia_editor --test book_tutorials` reads the `regia agent
call` lines below and runs them, in order, against a headless editor; a step the editor refuses,
or a `# expect` that does not hold, fails the test with this page's line number.

The graph format and every node kind are on [Material graphs](../agents/material-graph.md); the
panel's whole agent surface is on [Material graph editor](../agents/material-graph-editor.md).

## Step 1: a project and an empty graph

Make a project as in [your first scene](editor-first-scene.md#step-1-a-new-project-from-a-template),
and start a headless editor on it if an agent is driving:

```sh
regia new "First Material" --template blank
cd first_material
```

```text
regia editor --headless --project . > editor.out &
```

In the editor, open the **Material Graph** tab (it is in the default layout; **View ▸ Reset
layout** brings back a closed tab) and press **New** for an empty, unsaved graph; type
`assets/materials/painted_metal.matgraph.ron` in the path field next to it, which **Save as**
uses at the end. The agent call creates the graph with its path in one step:

```sh
regia agent call editor.matgraph_open '{"create":true,"path":"assets/materials/painted_metal.matgraph.ron"}'
```

## Step 2: a parameter and three nodes

A parameter is a value a material instance can override without recompiling. In the panel's
side column open **Parameters**, write the list as RON
(`[(name: "tint", value: Color((0.8, 0.2, 0.1)))]`, the form of the `.matgraph.ron` file) and
press **Apply**. Then right-click the canvas ▸ **Add
node** for a `Param` node reading `tint`, a `VertexColor` node, a `Swizzle` that keeps its
`rgb`, and a `Mul`. Adding a node has no keyboard shortcut; every node edit is one step on the
editor's undo stack.

```sh
regia agent call editor.matgraph_edit '{"op":"params","params":[{"name":"tint","value":{"Color":[0.8,0.2,0.1]}}]}'
regia agent call editor.matgraph_edit '{"op":"add","name":"tint_value","node":"Param(\"tint\")"}'
regia agent call editor.matgraph_edit '{"op":"add","name":"vcol","kind":"VertexColor"}'
regia agent call editor.matgraph_edit '{"op":"add","name":"rgb","node":{"Swizzle":{"input":"vcol","channels":"rgb"}}}'
regia agent call editor.matgraph_edit '{"op":"add","name":"base","kind":"Mul"}'
```

## Step 3: wire it

Drag from an output pin to an input pin to connect them: `tint_value` into the `a` slot of
`base`, `rgb` into `b`. Then wire `base` into the **Surface** box's `base_color` (or right-click
`base` ▸ **Drive output**). A slot left unwired is a compile error shown on its node.

```sh
regia agent call editor.matgraph_edit '{"op":"connect","from":"tint_value","to":"base","slot":"a"}'
regia agent call editor.matgraph_edit '{"op":"connect","from":"rgb","to":"base","slot":"b"}'
regia agent call editor.matgraph_edit '{"op":"output","slot":"base_color","from":"base"}'
regia agent call editor.matgraph_state  # expect /ok == true
```

## Step 4: compile, undo, redo

The panel compiles after every edit and shows **compiles** or the error count in its header.
`editor.matgraph_compile` runs the full check (the generated shader code is composed into the
forward pass) and answers the shader key and the parameters' slots:

```sh
regia agent call editor.matgraph_compile  # expect /ok == true
```

**Ctrl+Z** undoes the last edit (the base-color wire) and **Ctrl+Shift+Z** (or **Ctrl+Y**)
redoes it, in the panel as everywhere in the editor:

```sh
regia agent call editor.undo
regia agent call editor.matgraph_state  # expect /outputs/base_color == null
regia agent call editor.redo
regia agent call editor.matgraph_state  # expect /outputs/base_color == "base"
```

## Step 5: preview and save

The side column's preview sphere shows the material lit by the panel's light. It needs a GPU;
an agent on a machine with one reads the same image (base64 PNG and its mean color):

```text
regia agent call editor.matgraph_preview '{"width":256,"height":256}'
```

Press **Save as** (the path field's path; later saves use **Save**; the agent's
`editor.matgraph_save`). The graph is written next to a
`.matgraph.layout.ron` sidecar that holds only the canvas positions:

```sh
regia agent call editor.matgraph_save  # expect /ok == true
```

## What you built, and where next

`assets/materials/painted_metal.matgraph.ron`, a compiled graph with one parameter. Next:

- Material instances (`.matinst.ron`) override `tint` per object without a new shader:
  [Material graph editor § Tabs](../agents/material-graph-editor.md#tabs).
- [Package a build](package-a-build.md) ships the project with its materials.
- The [Material graph page of the editor guide](../user-guide/editor-guide/material-graph.md)
  lists every action of the panel.
