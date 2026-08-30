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
```

Use `status` for local state, `observe <path> --content <text>` for an explicit unsaved
overlay, and `remove <path>` to stage a deletion. Add `--json` to machine-readable
commands when passing results to another tool. Do not claim retrieval reflects current
files until `sync` has completed successfully.

## MCP

Run `oce-client-mcp` over stdio. The server exposes one tool,
`codebase-retrieval`. The MCP process indexes configured workspaces in the
background and incrementally synchronizes filesystem changes. Configure an MCP
host with explicit allowed workspaces and pass service credentials through the
environment:

```json
{
  "mcpServers": {
    "oce": {
      "command": "oce-client-mcp",
      "args": [
        "--workspace",
        "/path/to/workspace",
        "--initial-sync",
        "background"
      ],
      "env": {
        "OCE_API_URL": "http://127.0.0.1:8986",
        "OCE_API_KEY": "${OCE_API_KEY}"
      }
    }
  }
}
```

When the host cannot expand environment placeholders, configure the secret through its
secret manager instead of writing the literal key into this file. Pass
`information_request` to `codebase-retrieval`; pass `workspace_folder` when the
host has more than one configured workspace folder. Treat `status=indexing` as
a request to retry shortly, surface `status=error` to the user, and use
retrieval context only when `status=ready`.

MCP requires an explicit workspace configuration: use repeated `--workspace`,
`OCE_WORKSPACE`, or `OCE_WORKSPACES` (platform path separator). It never
silently indexes the process current directory. Service settings load in this
order: command argument, environment variable, then built-in default. API keys
are loaded from `OCE_API_KEY` only, never from command arguments.

When this skill is installed from a wheel, `oce-client skill install` copies the
bundled skill into `$CODEX_HOME/skills/oce-client` (or `$HOME/.codex/skills/oce-client`).
It does not overwrite an existing directory unless `--force` is supplied.
