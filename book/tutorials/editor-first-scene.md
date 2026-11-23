# Tutorial: your first scene in the editor

This tutorial builds a small scene in the editor without writing code: a new project from a
template, a ground, two meshes, a sun and a lamp, a save, and a play-in-editor run. Each step
says what to do with the mouse and keyboard, and gives the agent call that does the same thing,
so an agent (or a CI job) can follow the page too.

The page is a test. `cargo test -p regia_editor --test book_tutorials` reads the `regia agent
call` lines below and runs them, in order, against a headless editor; a step the editor refuses
fails the test with this page's line number. If you change a step, run that test.

For the same scene written as code, see [Your first scene](first-scene.md).

## Step 1: a new project from a template

In the editor: start it with no project (`regia editor`). The **Projects** window lists recent
projects and has a **New project** row: type `First Scene` in **Name**, the parent folder in
**Folder** (or **Browse…**), and press **Create blank project**. The editor writes the project
from the Blank template and opens its startup level, `assets/levels/main.level.ron`.

From a terminal, `regia new` writes the same files (the editor's `editor.new_project` agent
method does too):

```sh
regia new "First Scene" --template blank
cd first_scene
```

An agent then starts a headless editor on it and waits for its `ready` line
([Headless editor](../agents/headless-editor.md)):

```text
regia editor --headless --project . > editor.out &
```

Every `regia agent call` below talks to that editor. In the windowed editor the same calls work
while you watch: the editor answers on the endpoint in `.regia/agent/endpoint.json`.

## Step 2: place a ground and two meshes

In the editor: **Create ▸ Shape ▸ Cuboid**, three times. Each shape appears at the point the
viewport camera orbits, selected, so the Inspector shows its parameters (the **Primitive**
section: shape, size, color, roughness). Press **W** in the viewport for the move gizmo (**E**
rotate, **R** scale; hold **Ctrl** to snap), drag the handles, and press **F** to frame the
selection. Rename an entity from its Outliner row: right-click ▸ **Rename…**. Flatten the first
one into a ground (scale 20, 0.2, 20), stand the third up as a pillar.

The agent does the same with the editor's undoable methods: `editor.spawn` makes a named entity
with a transform (one undo step, and the guid a saved level needs), `editor.add_component` adds
the `Primitive` shape (a 1 m box by default) and `editor.set_property` changes one field:

```sh
GROUND=$(regia agent call editor.spawn '{"name":"Ground","translation":[0,-0.1,0],"scale":[20,0.2,20]}' | jq .entity)
regia agent call editor.add_component '{"entity":'"$GROUND"',"component":"Primitive"}'
CRATE=$(regia agent call editor.spawn '{"name":"Crate","translation":[0,0.5,0]}' | jq .entity)
regia agent call editor.add_component '{"entity":'"$CRATE"',"component":"Primitive"}'
regia agent call editor.set_property '{"entity":'"$CRATE"',"component":"Primitive","path":"color","value":[0.8,0.45,0.2,1.0]}'
PILLAR=$(regia agent call editor.spawn '{"name":"Pillar","translation":[2,1,0],"scale":[0.5,2,0.5]}' | jq .entity)
regia agent call editor.add_component '{"entity":'"$PILLAR"',"component":"Primitive"}'
```

Moving the crate with the gizmo is one undoable edit; `editor.set_transform` is the same edit,
and **Ctrl+Z** (`editor.undo`) takes it back:

```sh
regia agent call editor.set_transform '{"entity":'"$CRATE"',"translation":[0,0.5,-1.5]}'
regia agent call editor.undo
regia agent call editor.redo
```

## Step 3: light it

In the editor, create an empty entity (**File ▸ Create entity**, or the agent's
`editor.spawn`), then in the Inspector press **Add component** and pick `DirectionalLight` for a
sun or `PointLight` for a lamp. Its fields (direction, color, illuminance in lux, intensity in
lumens, radius) are edited in place.

```sh
SUN=$(regia agent call editor.spawn '{"name":"Sun"}' | jq .entity)
regia agent call editor.add_component '{"entity":'"$SUN"',"component":"DirectionalLight"}'
regia agent call editor.set_property '{"entity":'"$SUN"',"component":"DirectionalLight","path":"illuminance","value":100000.0}'
LAMP=$(regia agent call editor.spawn '{"name":"Lamp","translation":[-2,2,1]}' | jq .entity)
regia agent call editor.add_component '{"entity":'"$LAMP"',"component":"PointLight"}'
regia agent call editor.set_property '{"entity":'"$LAMP"',"component":"PointLight","path":"color","value":[1.0,0.8,0.6]}'
regia agent call scene.query_entities '{"name":"Lamp"}'
```

## Step 4: save

**File ▸ Save level** (or **Save level as…**) writes the open level. The agent call saves it to its last path (here
`assets/levels/main.level.ron`); pass `path` to save somewhere else:

```sh
regia agent call editor.save_level '{}'
```

## Step 5: play in the editor

Press **▶ Play** on the toolbar (or **Alt+P**). The editor plays a copy of the level; the edit
world is not touched, so whatever happens in play is gone when you stop. **⏸ Pause** pauses,
**⏭ Step** advances one frame while paused, and **⏹ Stop** (or **Escape**) ends the session.
**F8** possesses the game camera and gives it back.

The agent asks for the same, and waits on `editor.play_state` (each call is answered on an
editor frame, so this is a wait on the editor, not on a clock):

```sh
regia agent call editor.play
regia agent call editor.play_state  # until /mode == "playing"
regia agent call editor.pause
regia agent call editor.play_state  # until /mode == "paused"
regia agent call editor.step '{"frames":10}'
regia agent call editor.stop
regia agent call editor.play_state  # until /mode == "stopped"
```

A `# until P == V` comment means: repeat the call until the answer's value at the JSON pointer
`P` is `V`, so it follows only read calls (the page's test refuses an `# until` on a call the
schema marks mutating). A `# expect` comment checks the answer once and may follow any call.

## What you built, and where next

A level with a ground, a crate, a pillar, a sun and a lamp, saved, and played once. Next:

- [Your first material](editor-first-material.md): a material graph for those shapes.
- [Package a build](package-a-build.md): turn the project into something you can hand out.
- The [editor guide](../user-guide/editor-guide/index.md) lists every panel's actions,
  shortcuts and agent methods.
