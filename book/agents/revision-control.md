# Revision control

ADR-7837. Code: `regia_editor::vcs` (`git` driver, `VcsStatus` cache, `panel`, `agent`). Spec: the v0.6
editor addendum § 18.1-18.2 (spec 19).

The editor drives **git** through its own command-line program. Every call is an argument vector (never a
shell string), runs in the repository's top directory with `GIT_TERMINAL_PROMPT=0`, and handles no
credentials: git's own configuration (identity, hooks, signing) applies as it does in a terminal. The
program is `git` from `PATH`, or the preference `vcs.git_command` (a path or a name; only the owner sets
it: `editor.prefs_set` refuses it with `pref_owner_only`). Other systems are out of scope for v0.6; the
`RevisionControl` trait leaves room for them. A project that is not in a git work tree shows
**No revision control**: no badges, and every method below except `editor.vcs_status` answers
`no_revision_control`.

Paths in and out are **project-relative**, `/`-separated (`assets/props/crate.mesh.ron`). A project that
is a subdirectory of a larger repository sees only its own files, with its own paths.

## Where the state shows

- **Content Browser**: a list row ends with the path's letter in brackets, a tile shows it in its top-left
  corner (theme colors; the letter is always there, so color is never the only cue). The row menu's
  **Show history** opens the file's history in the Revision Control window.
- **Outliner**: a level row under the search box names the open level file and its letter (`·` when clean).
- **Status bar**: `⎇ <branch> · N changed` (or `clean`, or `No revision control`); a click opens the window.

| Letter | State | Meaning |
|---|---|---|
| `M` | `modified` | changed since the last commit (staged or not) |
| `A` | `added` | new and staged |
| `D` | `deleted` | deleted (staged or not) |
| `R` | `renamed` | renamed or copied (staged) |
| `?` | `untracked` | not tracked |
| `!` | `conflicted` | a merge conflict |
| `I` | `ignored` | matched by an ignore rule (a directory badges everything inside it) |

The badges come from a cache (`VcsStatus`). A background `git status --porcelain=v2` runs only when a change
notification moves its key: the project, the Content Browser's index generation, the asset watcher's reload
and error counts, the open level's path and saved state (a save makes the level clean, so a saved edit
shows `M` at once), or an explicit refresh (the window opening, its Refresh button). Idle frames start no
git process; `editor.vcs_status` reports `polls_started` and `polls_finished`. Every method below, and every
panel operation, refreshes the cache synchronously when it is done.

## The Revision Control window

View ▸ Revision Control, the palette command `view.toggle_revision_control`, or the status bar slot.

- **Branch** picker: choosing another branch switches to it; refused with `dirty_tree` while tracked files
  have staged or unstaged changes (untracked files travel with the switch). **New branch** + **Create
  branch** creates one at `HEAD` and switches to it.
- **Changes** tab: the **Staged** list (index against `HEAD`) with **Unstage**, and the **Changes** list
  (working tree: changed, deleted, untracked and conflicted files) with **Stage** and **Discard…**. Each row
  has a checkbox named by its path (the buttons act on the checked rows), its letter, and **Diff**.
  Discard asks first: it drops the unstaged changes (tracked files return to their staged or committed
  contents) and deletes untracked files; it cannot be undone and is not on the undo stack.
- **Diff**: a unified text diff (`+` lines in the theme's ok color, `-` lines in its error color) for text
  files; for binary files the sizes before and after. A RON asset or graph shows the structural,
  side by side diff of the asset diff window (`editor.asset_diff` returns the same changes) by default: a
  Staged row compares `HEAD` with the index, a Changes row compares the index with the working tree. The
  **Structural** and **Text** toggles beside the file name switch between it and the unified text. A RON
  asset missing on one side (added, deleted, untracked) or not valid RON shows the text with a note saying why
  there is no structural view.
- **The hooks other panels read.** Each answer of the status poll fills, once and without another git call:
  the status bar slot's text (`editor.status_bar` reports it as `revision: { label, tooltip }`, for example
  `main · 2 changed`), the revision line of the Content Browser's asset tooltip and `editor.asset_details`
  (`modified`, `untracked`, ...; `unchanged` for a path git does not list; `not tracked` without revision
  control) and the Outliner's Revision column.
- **Commit message** and **Commit**: the staged assets are validated first (below); then git commits the
  staged changes with the message (through stdin, so any length and any characters).
- **History** tab: the project's log, or one file's (following renames) after Show history; **All
  history** goes back to the project's. Columns: short id, date, author, summary.

Keyboard: every control is a focusable widget (Tab / Shift+Tab, Space or Enter presses, the picker opens
with Space); the text fields are named by their labels. The window scales with `appearance.ui_scale`.

### Validation before commit

Commit (the window's button and `editor.vcs_commit`) runs the project's asset validators
(`config/validation.ron`, as the Content Browser's Validate does) on the staged files that are assets of
the loaded registry (deletions are skipped). An error refuses the commit with `validation_failed` and hands
the report to the Problems panel; warnings refuse it with `validation_warnings` until the window's **Commit
anyway** (or `allow_warnings: true`). With no registry loaded nothing is validated and `validated` is empty.
git's own hooks still run after that, as configured.

## Methods

| Method | Params | Answers |
|---|---|---|
| `editor.vcs_status` | — | `{ state, system, branch, detached, head, files: [{ path, orig_path, state, staged, unstaged }], staged: [path], unstaged: [path], changed, polls_started, polls_finished, error }`; `state` is `ready`, `no_project`, `no_revision_control` or `unavailable` (then `error: { code, message }`) |
| `editor.vcs_stage` | `paths` | `{ staged, unstaged }` after staging (new, changed and deleted files alike) |
| `editor.vcs_unstage` | `paths` | `{ staged, unstaged }`; the working-tree changes stay |
| `editor.vcs_discard` | `paths`, `confirm: true` | `{ discarded, staged, unstaged }` |
| `editor.vcs_commit` | `message`, `paths?` (staged first), `allow_warnings?` | `{ commit, validated, warnings }` |
| `editor.vcs_diff` | `path`, `staged?` | `{ path, staged, kind, text, old_size, new_size, truncated }`; `kind` is `text`, `binary` or `unchanged`; an untracked file is all `+` lines; text over 512 KiB is cut (`truncated`) |
| `editor.vcs_log` | `path?`, `limit?` (default 100, at most 500), `open?` | `{ path, entries: [{ id, author, email, date, summary }] }`, newest first; `open: true` also shows it in the window's History tab |
| `editor.vcs_branch` | `action` (`list`, `switch`, `create`), `name?`, `switch?` (create; default true) | `{ current, branches }` |

`editor.vcs_status`, `editor.vcs_diff` and `editor.vcs_log` are read-only; the others need the `assets`
permission.

```json
{"method": "editor.vcs_stage", "params": {"paths": ["assets/a.txt", "assets/new.txt"]}}
{"method": "editor.vcs_commit", "params": {"message": "Tune the crate material"}}
{"method": "editor.vcs_log", "params": {"path": "assets/a.txt"}}
{"method": "editor.vcs_branch", "params": {"action": "create", "name": "feature/lighting"}}
```

## Error codes

| Code | When |
|---|---|
| `no_project` | no project is open |
| `no_revision_control` | the project is not in a git work tree |
| `git_not_found` | the git program is missing (install git or set `vcs.git_command`) |
| `git_failed` | git exited with an error (its first lines are in the message) |
| `invalid_path` | a path is empty, absolute, contains `..`, starts with `-`, or `paths` is missing |
| `nothing_staged` | a commit with an empty index |
| `empty_message` | a commit without a message |
| `conflicts` | a commit, or a discard of a conflicted path, while a merge conflict is open |
| `confirm_required` | `editor.vcs_discard` without `confirm: true` |
| `validation_failed` | a staged asset failed validation; the commit was not made |
| `validation_warnings` | validation reported warnings and `allow_warnings` was not set |
| `invalid_branch` | a branch name git refuses (or one starting with `-`), or a missing `name` |
| `unknown_branch` | switch to a branch that does not exist |
| `branch_exists` | create a branch that exists |
| `dirty_tree` | switch while tracked files have changes |
| `invalid_action` | `editor.vcs_branch` with an action other than `list`, `switch`, `create` |

## Not in v0.6

Push, pull and fetch (git's own tools and credentials do that), and systems other than git. File locking
is below.

## File locks (git LFS)

For projects that use git LFS file locking, the editor locks a binary asset when you start to edit it, shows
who holds each lock, and refuses to overwrite a file someone else locks (ADR-7864).

**When it is available.** Locking is on when the repository's attributes mark paths `lockable`
(`.gitattributes` at the repository top or in the project directory, or `.git/info/attributes`), for
example `*.psd filter=lfs diff=lfs merge=lfs -text lockable`, and `git lfs version` runs. Otherwise the
File Locks window and `editor.vcs_locks` say "not available" with the reason, no lock is ever asked for, no
badge shows and no save is blocked.

**Lock on edit.** The first edit that makes a lockable file unsaved (in any asset editor, or the level)
reads the lock list once, then:

- someone else holds it: a warning says saving will be blocked;
- otherwise the preference `vcs.lock_on_edit` decides: `ask` (default) shows "Lock this file?" with Lock and
  Not now; `always` locks it at once (`git lfs lock`); `never` does nothing.

It happens once each time the file turns unsaved; a file you already lock is never asked for again.

**Badges.** Content Browser rows show `[Locked by you]` or `[Locked by <name>: saving is blocked]`; tiles show
`L` (yours) or `X` (someone else's) in the top-right corner. With the preference `vcs.lockable_read_only`,
lockable files you do not lock show `[Read-only until you lock it (lockable)]` and `RO`.

**Save guard.** Saving the level (File ▸ Save, `editor.save_level`) or the other documents with Save all, to a
file someone else locks, is refused with `locked_by_other`; the file is not touched. The level's prompt offers
**Save anyway as a copy**: the level is written next to it as `<name> (copy).level.ron` (then `(copy 2)`, ...),
and the open level keeps its path. Agents save a copy with `editor.save_level { path }`.

**Unlock.** The Content Browser's context menu has Lock and Unlock on lockable files. After a commit, the
committed files you still lock are offered in the File Locks window (**Unlock committed files**); **Unlock
all mine** (the window, or the palette command `vcs.unlock_all_mine`) releases every lock you hold. Another
user's lock is never forced.

**File Locks window.** Palette `view.toggle_file_locks`: the lockable patterns, every lock with its owner,
Refresh, Unlock per row of yours, Unlock all mine. Every control is a focusable widget (Tab, Space/Enter).

The lock list is read in the background when the project opens, and on demand (first edit, Refresh,
`editor.vcs_locks`); nothing runs per frame.

| Method | Params | Result |
|---|---|---|
| `editor.vcs_locks` | `paths?` (answered in `query`), `refresh?` (default true: read the list from git LFS first) | `{ support, reason, patterns, locks: [{ path, id, owner, mine, locked_at }], mine, theirs, unlock_offer, prompts: [{ kind, path, owner }], lock_calls, unlock_calls, query: [{ path, lockable, locked, owner, mine, read_only }] }`; `support` is `available`, `unavailable` (with `reason`) or `unknown` |
| `editor.vcs_lock` | `paths` | `{ locked: [{ path, id, owner, mine, locked_at }] }`; a file you already lock is answered without a new call |
| `editor.vcs_unlock` | `paths`, or `all_mine: true` | `{ unlocked: [path] }` |

`editor.vcs_locks` is read-only; `editor.vcs_lock` and `editor.vcs_unlock` need the `assets` permission.

```json
{"method": "editor.vcs_locks", "params": {"paths": ["assets/art/hero.psd"]}}
{"method": "editor.vcs_lock", "params": {"paths": ["assets/art/hero.psd"]}}
{"method": "editor.vcs_unlock", "params": {"all_mine": true}}
```

### Lock error codes

Besides `no_project`, `no_revision_control`, `git_not_found` and `invalid_path` above:

| Code | When |
|---|---|
| `locking_unavailable` | no path is marked `lockable`, or git LFS is not installed |
| `not_lockable` | lock of a file no `lockable` pattern matches |
| `locked_by_other` | lock of a file someone else holds, or a save (`editor.save_level`, File ▸ Save, Save all) to it |
| `not_locked_by_you` | unlock of a file you do not lock (other users' locks are never forced) |
| `lfs_failed` | git LFS exited with an error (its message is included), or answered unreadable JSON |
