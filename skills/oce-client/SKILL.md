---
name: oce-client
description: "Use the oce-client CLI or MCP server to synchronize a workspace and retrieve indexed code from OpenContextEngine."
---

# Oce Client

Use this skill when a task needs repository code context from an OpenContextEngine service.

## Configuration

Set `OCE_WORKSPACE` before invoking the client. The defaults are
`OCE_API_URL=http://127.0.0.1:8986` and `OCE_API_KEY=sk-opencontextengine`; set
those environment variables only when connecting to a different service or key.
Keep API keys in the environment; never put
them in prompts, command output, source files, or committed configuration.

The CLI is installed as `oce-client`. Global options must precede the command:

```text
oce-client --root <workspace> sync
oce-client --root <workspace> retrieve "where is authentication implemented?"
oce-client --root <workspace> paths "where is authentication implemented?"
oce-client --root <workspace> overview --depth basic
```

Use `status` for local state, `observe <path> --content <text>` for an explicit unsaved
overlay, and `remove <path>` to stage a deletion. Add `--json` to machine-readable
commands when passing results to another tool. Do not claim retrieval reflects current
files until `sync` has completed successfully.

## MCP

Run `oce-client-mcp` over stdio, or use `oce-client mcp`. The server exposes one
tool, `codebase-retrieval`. Each call automatically initializes local state,
scans and synchronizes the selected workspace, then retrieves current code
context. Configure an MCP host with a stdio command and pass the same
environment variables:

```json
{
  "mcpServers": {
    "oce": {
      "command": "oce-client-mcp",
      "env": {
        "OCE_API_URL": "http://127.0.0.1:8986",
        "OCE_API_KEY": "${OCE_API_KEY}",
        "OCE_WORKSPACE": "/path/to/workspace"
      }
    }
  }
}
```

When the host cannot expand environment placeholders, configure the secret through its
secret manager instead of writing the literal key into this file. Pass
`information_request` to `codebase-retrieval`; pass `workspace_folder` when the
host has more than one workspace folder open. Synchronization failures should be
surfaced to the user.

When this skill is installed from a wheel, `oce-client skill install` copies the
bundled skill into `$CODEX_HOME/skills/oce-client` (or `$HOME/.codex/skills/oce-client`).
It does not overwrite an existing directory unless `--force` is supplied.
