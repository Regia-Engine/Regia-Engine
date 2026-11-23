# Hosted Agent panel

Open **Window → Agent**, or run `editor.command {"run":"window.open_agent"}`
from an agent or the keyboard command palette. Nothing starts until you configure
an executable in Preferences → Agents → Agent commands and press Start.
The panel never contacts any service by itself. A chosen executable may contact
services using its own configuration; no provider or credentials are installed.

## Configure a command

The `agents.commands` preference is a JSON list of names and argument vectors:

```json
[{"name":"Local assistant","command":["/path/to/my-agent","--regia-stdio"]}]
```

Edit this setting in Preferences. Like the code-editor executable setting, it is
owner-only: `editor.prefs_set` refuses it with `pref_owner_only`. There is no shell expansion. An executable
path containing spaces is one argument. Configure credentials in the external
agent's environment or its own credential manager, never in this preference.
Credential-bearing argument names and recognizable credential text are refused.

The child runs in the open project directory, with the editor user's normal OS
privileges. Modes restrict its **agent connection**, not arbitrary file or network
access by the executable. Only run commands you trust with your project.

## Session operations

`editor.agent_sessions` takes the following parameters. The method requires the
project's `tools` permission, including for `list`; the human panel calls the same
operation in-process. No hosted child or scoped tool may change sessions or approve
its own work through this method.

| Parameter | Meaning |
|---|---|
| `action` | `list` (default), `start`, `stop`, `send`, `mode`, or `staged` |
| `command` | Configured display name, required for `start` |
| `session` | Numeric session id, required for `stop`, `send`, `mode`, and `staged` |
| `mode` | `ask` (default for start), `propose`, or `act` |
| `text` | `send` message, 1–8192 bytes |
| `approve_plan` | `send` a boolean plan decision instead of `text` |
| `approve` | `staged`: `true` runs the chosen staged calls, `false` discards them |
| `items` | `staged`: staged call ids to decide, at most 32 (default: all of them) |

```json
{"action":"start","command":"Local assistant","mode":"ask"}
{"action":"send","session":1,"text":"Describe the selected entity."}
{"action":"mode","session":1,"mode":"act"}
{"action":"send","session":1,"approve_plan":true}
{"action":"staged","session":1,"approve":true,"items":[2]}
{"action":"staged","session":1,"approve":false}
{"action":"stop","session":1}
```

`list` returns `{commands,sessions}`. Other actions return the affected session:
`{id,name,mode,running,messages,plan,history,history_error,staged}`. A message has
`{mode,role,text}`; every entry keeps its reception-time mode. A plan has
`{steps,approved,revision}`. A staged call has `{id,method,params,preview}`; see
[Propose and staged calls](#propose-and-staged-calls). No response includes tokens, environment variables or command
arguments. History lives in `.regia/agent_sessions/<pid>-<timestamp>-<id>.jsonl`;
each row has `format_version: 1`, `agent`, `session`, and `message`. The latest
256 messages are kept per session, written using a temporary file and rename.
Existing files remain available after restart; old sessions are not restarted.
`history` is a display path: invalid Unicode uses replacement characters, while
file operations retain the native OS path.

## Modes and plans

- **Ask only** permits the Read permission class. `editor.select` is refused with
  `F0012` and leaves the selection untouched.
- **Act** uses the project's normal permissions. It cannot control agent sessions.
  The Review permission is absent by default; granting it explicitly retains its
  normal effect in Act. Each call retains the standard parameter validation,
  rate limits, audit and change provenance.
- **Propose** allows text-file proposals through `review.propose`, accepted in
  **Agent Changes**, and `review.reject`. Live `editor.*` edits are staged for
  approval instead of running (below). Read calls run immediately.

An arriving plan closes the mutation gate until the user approves it. Read calls
remain available. Changing modes does not approve a plan; rejecting it leaves the
gate closed. A new plan replaces the pending or approved one. The checklist is
keyboard-operable; review each step and press Approve plan, or Reject plan.

## Propose and staged calls

In Propose mode, a call to a synchronous `editor.*` method of the EditScene or Assets
permission class does not run. After the usual permission and plan checks it is
**staged**: the editor keeps the method, its parameters and a preview, and answers
the agent at once:

```json
{"staged":true,"staged_id":3,"pending":1,
 "preview":{"summary":"Create an entity (name, transform, optional parent) as one undoable entry, like the outliner's create",
            "permission":"edit_scene","touches":["new entity \"Crate\""]}}
```

`preview.touches` lists what the call would touch, read from its parameters
without running it: entities named by id (`entity 42 "Lamp"`, or `(missing)`),
project files named by path, a new entity for create-style methods, or
`the current selection` when an entity method names none. It is a guide, not a
dry run: the method's own validation runs only when the call runs.

The panel lists the staged calls under **Staged editor calls**. Check calls with
**Include staged call**, then press **Approve checked calls** or **Reject checked
calls**, or decide every call with **Approve all staged calls** or **Reject all
staged calls**. The protocol equivalent is `action: "staged"` with `approve` and
optional `items`.

- **Approve** runs the chosen calls in the order they were staged, through the
  same dispatcher, permission checks, audit and provenance as a live call, inside
  one undo transaction labeled `Agent proposal (N calls)`: one Undo removes all of
  them. If one call fails, the calls before it are reverted, none stay applied,
  and the decision fails with `session_staged_failed` naming the call. Calls not
  chosen stay staged.
- **Reject** discards the chosen calls; nothing runs and the world is unchanged.

Read-only calls are never staged, so they see the world without the staged edits.
A later staged call cannot use an id that an earlier staged call would create.
The queue is session-only: it is never written to disk, holds at most 32 calls
(more are refused with `session_staged_full`), and is discarded when the session
stops for any reason. Switching away from Propose keeps the queue for the owner to
decide. Calls that manage history or replace the world (`editor.undo`,
`editor.redo`, `editor.history_jump`, the `editor.transaction_*` calls, opening
levels, projects or prefabs, `editor.command` and `editor.menu_invoke`), deferred
methods, and calls outside `editor.*` (for example `scene.spawn`) cannot be staged
and still return `session_propose_live_unsupported`. Approval needs no open undo
transaction; with one open it returns `session_staged_busy`.

## Child protocol, version 1

The child receives `REGIA_AGENT_LINE_PROTOCOL=1`, `REGIA_TOOL_ENDPOINT` (private
loopback WebSocket URL), `REGIA_TOOL_TOKEN` (in-memory bearer token), and
`REGIA_TOOL_ID`. A Rust adapter can use
`regia_agent::client::Client::connect_tool_from_env`. The master token is never
passed to the child. The token is revoked on Stop, output failure, project change,
child exit, and editor shutdown.

Stdout is UTF-8 JSON, one object per line, flushed after each object:

```json
{"kind":"message","text":"I found three lights."}
{"kind":"plan","steps":["Select the light","Adjust its intensity"]}
```

Other output is ignored; stderr is discarded. A recognized `message` or `plan`
object with invalid fields stops the session. A message is at most 8192 bytes;
a whole line at most 16384 bytes; a plan has 1–32 nonempty steps of at most 512
bytes each. Oversized lines or queue overflow stop the session. Tokens and values
of credential-named environment variables are redacted before display or storage;
recognizable credential-bearing lines are omitted. Do not print credentials:
arbitrary secrets cannot be inferred reliably from prose.

Stdin also carries flushed JSON lines:

```json
{"kind":"mode","mode":"ask"}
{"kind":"message","mode":"act","text":"Proceed with the selection."}
{"kind":"plan_pending","mode":"act"}
{"kind":"plan_decision","mode":"act","approved":true}
{"kind":"staged_decision","mode":"propose","approved":true,"items":[2,3]}
```

The first line reports the initial mode. Wait for `plan_pending` after emitting a
plan, then for `plan_decision` with `approved: true` before making mutation calls.
The editor enforces the pending-plan gate independently of child cooperation once
the plan arrives. `staged_decision` reports the owner's decision on staged calls:
`approved` is `true` only if the calls ran; a failed approval or a rejection is
`false`. Protocol calls still use the WebSocket connection, not stdout.
While a session's stdout holds lines the editor has not read yet, its next call
waits until they are read, so a plan printed before a call always gates that call.
Stop revokes the token before killing and reaping the process group on Unix or the
job object on Windows, using the same platform helpers as editor launches.

## Keyboard and limits

Tab and Shift+Tab traverse the command picker, Start, session picker, Stop, mode
buttons, plan checklist and decisions, staged-call checks and decisions, message
field and Send. Space toggles a focused check. Enter in the message
field sends once. Buttons also use standard keyboard activation. Controls inherit
appearance scaling and theme tokens; there are no icon-only buttons. The panel
guards side effects with egui's cumulative frame number.

At most 16 sessions are retained; a stopped session is evicted before the next
start at capacity. Input and output queues are bounded. Idle ticks do not write
history or request repaint. Output and protocol arrivals wake the editor.

## Errors

| Code | Meaning |
|---|---|
| `session_bad_action` | Unknown action |
| `session_bad_commands` | Invalid command list or missing command name |
| `session_bad_message` | Missing, empty or oversized text |
| `session_bad_mode` | Mode is not ask, propose or act |
| `session_bad_plan` | No pending plan or invalid decision |
| `session_bad_staged` | Nothing staged, `approve` not a boolean, or `items` names an unknown call |
| `session_busy` | Child input queue is full or closed; retry after it drains |
| `session_credentials` | Credentials found in command arguments |
| `session_io` | Cannot create listener or history |
| `session_limit` | All 16 sessions are still running |
| `session_no_project` | No project is open |
| `session_not_configured` | Command name is not configured |
| `session_not_found` | Missing or unknown session id |
| `session_owner_only` | Hosted callers cannot approve work or control sessions |
| `session_plan_pending` | The current plan awaits approval |
| `session_propose_live_unsupported` | Propose cannot stage this call (see above) |
| `session_spawn` | Executable, pipes, worker, or process containment failed |
| `session_staged_busy` | An undo transaction is open; approve after it ends |
| `session_staged_failed` | An approved call failed; none of the approved calls stay applied |
| `session_staged_full` | 32 calls are already staged in this session |
| `session_stopped` | The session has ended |

`F0012` is the existing permission-denied code. Revoked connections use the existing
`tool_token_revoked` response. Usual envelope, parameter and rate-limit errors apply.

## Verification

`cargo test -p regia_editor --lib agent_sessions` launches a Rust fixture process
that reads stdin and calls `editor.select` over the real connection. It checks
Ask-only refusal, Act selection, plan locking, project permission intersection,
file review, history redaction, working directory, token revocation and idle work.
`propose_mode_stages_a_spawn_without_changing_the_world`,
`approving_runs_the_staged_calls_as_one_undo_step` and
`rejecting_leaves_the_world_unchanged` compare scene snapshots around staging,
approval with one Undo, and rejection; further tests cover per-item decisions and a
failing call reverting the group.
`cargo test -p regia_editor --lib agent_panel` checks keyboard Enter at 1.5x scale
and approving a checked staged call from the keyboard in a two-pass frame.
`cargo test -p regia_editor --test agent_sessions` pins the disabled default and
registered panel/preferences surface.
