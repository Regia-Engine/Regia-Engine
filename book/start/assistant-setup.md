# Set up with your AI assistant

You can let an AI coding assistant install Regia Engine for you: open the engine folder (or paste the repository's
URL, https://github.com/Regia-Engine/Regia-Engine) in any assistant that can read files and run commands, and say
*"Set Regia up for me."* This page says what happens then, what the assistant will and will not do, and how to do
it by hand instead.

The assistant reads `AGENTS.md` at the root of the engine folder, the instructions Regia ships for assistants. Any
assistant works, including one that runs a local model; Claude Code also picks up the project skills in
`.claude/skills/`. Regia itself always runs on your machine and needs no account.

## What happens

1. **Introduction.** The assistant says in a few sentences what Regia is, names the three ways of working with
   it, and says it will ask about six questions.
2. **Two questions before anything is downloaded**: whether to set Regia up on this machine (it names the OS, CPU,
   memory, GPU and free disk it sees), and whether to get the engine as a git clone or a release archive, and into
   which folder. Either way Regia is built from source on your machine.
3. **Prerequisites.** A C/C++ compiler and some system libraries with a Vulkan driver (Linux), the Visual Studio
   Build Tools (Windows) or the Xcode command-line tools (macOS). These need administrator rights, so the assistant
   shows you the exact command and why, and you run it.
4. **Rust and Regia.** With your consent for each download, and after telling you its size and where it comes
   from, the assistant installs Rust for your user account (checking the installer against its published hash),
   gets the engine source, and builds the `regia` tool and the editor. The first build downloads about 150 MB of
   crates, uses about 10 GB of disk and takes 20 to 60 minutes.
5. **The check.** `regia doctor` checks the toolchains and the GPU driver; the assistant shows you the fix for
   anything it reports.
6. **Four more questions**: which platforms to build games for, how you want to work with the assistant (below),
   which template to start from, and the project's name and folder.
7. **Your first project.** It creates the project from the template, builds it, runs it once and opens the editor.

Each question is asked once and labeled, for example "(Question 3 of 6 · targets)". Say "you decide" to any
question and the assistant takes the default and tells you which. If you stop part-way, nothing breaks: say
"continue Regia setup" later and it picks up from what is already built.

| Question | Default |
|---|---|
| (Question 1 of 6 · machine) set Regia up on this machine | yes |
| (Question 2 of 6 · install_from) git clone or release archive, and the engine folder | a git clone in `~/Regia/regia-engine` |
| (Question 3 of 6 · targets) platforms to build games for | this machine only |
| (Question 4 of 6 · mode) how you want to work | 1, local-first |
| (Question 5 of 6 · template) the first project's template | `first_person` |
| (Question 6 of 6 · project) project name and folder | `My Game` in `~/RegiaProjects` |

## How you work together

| Mode | Who drives | What the assistant does |
|---|---|---|
| 1 Local-first (default) | you | installs, explains what is on screen, points at the manual, runs the commands you ask for; does not connect to the editor or change your project unasked |
| 2 Co-pilot | you | also connects to your running editor over MCP to look, take screenshots, play-test and make the edits you ask for |
| 3 Assistant builds | the assistant | builds the game through the editor (code, content, tests, screenshots) and asks you only for decisions; keeps a short decision log in your project |

In modes 2 and 3 every edit the assistant makes goes onto the editor's undo stack and shows in the Agent Changes
panel, so you can see and revert it. Golden images stay yours: only you accept one, with `regia golden accept` at a
terminal. Switch modes at any time by saying so ("connect to my editor", "take over", "stop touching my project").

Connecting (modes 2 and 3) writes one entry into your project's MCP file (for Claude Code, `<project>/.mcp.json`)
and nothing into your assistant's own settings. Your assistant then asks you to start a new session in the project
folder and to approve the `regia` server there.

## What the assistant will and will not do

It will:

- ask before every download, saying what, from where and how large;
- show you every command that needs administrator rights, with the reason, for you to run;
- change files only in the engine folder and your projects folder, and ask separately before anything else (the
  `PATH` line in your shell profile, the project's MCP file);
- check the Rust installer against its published hash before running it.

It will not:

- accept any license or terms for you (Regia's own license, the Mozilla Public License 2.0, needs no acceptance to
  use or build it), or click Regia's windows for you;
- run `sudo` or any other elevation, or pipe a downloaded script into a shell;
- edit the engine's own source unless you ask.

## Privacy

Regia sends nothing anywhere on its own: no telemetry service, no update check, no account. (A game you make can
upload its crash reports to your own server, but only when you turn that on for it:
[Crash reports](../agents/crash-reports.md).) The only network traffic setup makes is the downloads you agreed to (the repository, the Rust distribution server, crates.io during the
first build, and the Android SDK repository if you chose Android).

What you type to your assistant, and what it reads (including `regia doctor` output), goes wherever your assistant
sends it; an assistant running a local model keeps it on your machine. The `regia` MCP server listens on your
machine only ([MCP over HTTP](../agents/mcp-http.md)).

## Uninstall

Setup puts nothing outside the folders you chose and the changes you agreed to. Delete the engine folder to remove
Regia (your projects stay, but they need the engine folder to build), remove the `PATH` line from your shell profile
if you let the assistant add one, and delete the `regia` entry from a project's MCP file. Rust (`rustup self
uninstall`) and system packages are yours to keep or remove.

## Doing it by hand

Everything the assistant does is in [Installing Regia Engine](install.md), then
[Your first project](../getting-started.md). For assistants and the curious, the full flow, the consents and the
fixes for common failures are in [Onboarding: setting Regia up (agents)](../agents/onboarding.md).
