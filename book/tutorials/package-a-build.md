# Tutorial: package a build

This tutorial takes a project from the editor to a package you can hand out: set the version in
**Project Settings**, check the assets, save, close the editor, and let `regia package` show
what it would ship before it builds anything. The full reference is [Packaging](../user-guide/packaging.md).

The page is a test, in two halves. `cargo test -p regia_editor --test book_tutorials` runs the
`regia agent call` lines against a headless editor; `cargo test -p regia_cli --test
book_tutorials` runs the `regia new`, `cd` and `regia package` lines through the real `regia`
binary. A step that fails names this page's line.

## Step 1: a project with something in it

```sh
regia new "Ship It" --template blank
cd ship_it
```

```text
regia editor --headless --project . > editor.out &
```

In the editor, place a shape (**Create ▸ Shape ▸ Cuboid**) and save the level
(**File ▸ Save level**):

```sh
regia agent call scene.spawn_primitive '{"shape":"cuboid","name":"Crate","transform":{"translation":[0,0.5,0]}}'
regia agent call editor.save_level '{}'
```

## Step 2: name and version

Open **Edit ▸ Project settings…** (the **Project Settings** tab). The top rows are the project's
**Name**, **Id**, **Version**, **Build number** and **Organization**, and **Default level:**
picks the level a package starts on. Change **Version** to `1.0.0` and **Build number** to `2`,
then press **Save**: the panel writes `Regia.toml` and keeps every key it does not show.

`editor.project_settings_set` is the same edit and the same validation (nothing is written when
a value is refused); `editor.project_settings_get` reads what the panel shows:

```sh
regia agent call editor.project_settings_get  # expect /project/name == "Ship It"
regia agent call editor.project_settings_set '{"project":{"version":"1.0.0","build_number":2}}'
regia agent call editor.project_settings_get  # expect /project/version == "1.0.0"
```

The **Platforms and quality presets** rows of the same panel turn targets on and off and pick
each one's render profile (`editor.project_settings_set` with `platforms`).

## Step 3: check the assets

In the **Content** tab, **Validate project** runs the asset validators (missing references,
naming rules, texture sizes, duplicate ids, orphans, triangle budgets) over the cooked asset
registry and lists each finding. A shipping package refuses some of the same problems, so fix
them here. The agent lists the validators and their limits:

```sh
regia agent call editor.asset_validate '{"list":true}'
```

and, once the project has been cooked (step 5's dry run cooks it, as does `regia cook`), runs
them over the registry. Before the first cook that call answers `registry_not_loaded`:

```text
regia agent call editor.asset_validate '{"source":"registry"}'
```

## Step 4: close the editor

Packaging reads the files on disk, so save and close the editor first. `agent.shutdown` saves
every document with edits and ends a headless editor with exit status 0:

```sh
regia agent call agent.shutdown
```

## Step 5: see what would ship

`--dry-run` cooks the content if it is missing or stale, prints the size report of the files the
profile ships, and stops: no compile, no package. It is the cheap check before a long build:

```sh
regia package --platform linux --dry-run
```

## Step 6: build the package

The real build compiles the game for the platform and writes the package under
`dist/<platform>/` (the test does not run these: they compile the whole game):

```text
regia package --platform linux                       # a dev package (tar, per [platforms.linux] formats)
regia package --platform linux --profile shipping    # no agent server, no dev tools
regia package --platform windows                     # a zip; build it on Windows or with the cross tools
regia package --platform android                     # an APK; needs the Android SDK and NDK (regia doctor)
regia package --platform linux --steam               # adds the Steam depot layout and steamcmd files
```

[Packaging § Smoke-test a package](../user-guide/packaging.md#smoke-test-a-package-before-shipping)
starts the package and checks it through the agent protocol before you ship it.

## What you did, and where next

A versioned project, checked and dry-run packaged. Next:

- [Build profiles](../user-guide/packaging.md#build-profiles) ship one level's content only.
- [Steam](../user-guide/steam.md) and [Steam Deck](../user-guide/steam-deck.md) for a store build.
- The [Project Settings page of the editor guide](../user-guide/editor-guide/project-settings.md).
