# Agent

The **Agent** tab runs an agent program you choose, inside the editor, and lets you talk to it. The editor installs no
agent, no provider and no credentials: you configure the command of an agent you already have, and the panel starts
it in the open project's folder and connects it to the editor's agent protocol. A mode decides what the agent may do:
only read (**Ask only**), propose file changes you review (**Propose**), or edit with the project's normal permissions
(**Act**). Depth: [Hosted Agent panel (agents)](../../agents/agent-panel.md), [Agent Changes](agent-changes.md).

## Open it

Choose **Window ▸ Agent**. From the keyboard press Ctrl+Shift+P and pick **Open the Agent tab** (`window.open_agent`).
Nothing starts when the tab opens.

## Configure a command

In **Preferences ▸ Agents ▸ Agent commands**, add a name and the program with its arguments, for example the name
"Local assistant" and the command `/path/to/my-agent --regia-stdio`. Each argument is passed as written: there is no
shell, so no variables or wildcards are expanded. Keep the agent's credentials in its own environment or credential
manager: the editor refuses argument names and text that look like credentials. Only an editor user can change this
setting; an agent cannot.

The program runs with your normal operating-system rights in the project folder. The mode limits what it can do through
the editor's agent connection, not what the program itself can do with files or the network: only run commands you
trust with your project.

## Run a session

1. Pick a configured command and press **Start**. Each start is a new session; the session picker switches between
   them, and **Stop** ends the selected one. At most 16 sessions are kept; a stopped one makes room for the next.
2. Pick the mode: **Ask only**, **Propose** or **Act**. Changing the mode never approves a plan.
3. Type in the message field and press **Send** (or Enter in the field). The agent's answers appear in the
   conversation, each marked with the mode it arrived in.
4. When the agent sends a plan, its steps appear as a checklist and the agent cannot make changes until you press
   **Approve plan**; **Reject plan** keeps it blocked. Reading stays allowed meanwhile. A new plan replaces the old
   one.
5. In **Propose**, the agent's file changes wait in [Agent Changes](agent-changes.md) for you to accept or reject.

Each session's conversation is kept in `.regia/agent_sessions/` (the newest 256 messages). After the editor restarts,
earlier conversations can still be read, but their sessions are not started again.

## Actions and agent methods

| Action | Control / key | Agent method |
|---|---|---|
| List commands and sessions | The command and session pickers | `editor.agent_sessions {"action": "list"}` |
| Start a session | **Start** | `editor.agent_sessions {"action": "start", "command", "mode"}` |
| Send a message | Message field, **Send** or Enter | `editor.agent_sessions {"action": "send", "session", "text"}` |
| Change the mode | **Ask only**, **Propose**, **Act** | `editor.agent_sessions {"action": "mode", "session", "mode"}` |
| Approve or reject a plan | **Approve plan**, **Reject plan** | `editor.agent_sessions {"action": "send", "session", "approve_plan"}` |
| Stop a session | **Stop** | `editor.agent_sessions {"action": "stop", "session"}` |

Tab and Shift+Tab move through the command picker, **Start**, the session picker, **Stop**, the mode buttons, the plan
checklist and its decisions, the message field and **Send**. The session methods need the project's `tools`
permission, and a hosted agent can never control sessions or approve its own plan through them.
