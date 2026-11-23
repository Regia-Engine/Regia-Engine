# MCP client configuration template

`regia.mcp.json.example` is the shape of the project-scope MCP file Claude Code reads from a project folder
(`<project>/.mcp.json`). Copy the `regia` entry into your game project's `.mcp.json` (keep any other servers in
that file) and replace both placeholder paths with absolute paths: an assistant's environment may not have your
`PATH`. Claude Code asks you to approve a project server before it uses it. Other MCP clients take the same
command line in their own settings.

```sh
regia mcp --project /absolute/path/to/your/project              # stdio: one MCP tool per agent-protocol method
regia mcp --project /absolute/path/to/your/project --headless   # also starts a windowless editor when none runs
regia mcp --project /absolute/path/to/your/project --http       # Streamable HTTP on a loopback port, with a bearer token
```

With `--http` the URL and the token file are written to `<project>/.regia/agent/mcp_http.json`.

After the file is written, start a new session of your assistant in the project folder and approve the `regia`
server when it asks. To disconnect, delete the `regia` entry. More:
[Agent protocol](../../book/user-guide/agent-protocol.md), [MCP over HTTP](../../book/agents/mcp-http.md),
[Headless editor](../../book/agents/headless-editor.md).
