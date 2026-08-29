# opencontextengine-client

Standalone synchronous Python client for OpenContextEngine workspace and blob
management. The package owns local inventory, ignore rules, upload planning,
checkpoint state, and retrieval adapters. It does not depend on Auggie SDK.

Install the distribution package with `uv add opencontextengine-client` (or
`pip install opencontextengine-client`). The installed command remains
`oce-client`.

## CLI

Install the package with `uv` and configure the service endpoint and key through
the environment:

```powershell
# These are the built-in defaults; override them only when needed.
$env:OCE_API_URL = "http://127.0.0.1:8986"
$env:OCE_API_KEY = "sk-opencontextengine"
$env:OCE_WORKSPACE = (Get-Location).Path
uv run oce-client sync
uv run oce-client retrieve "where is request authentication implemented?"
```

If unset, `OCE_API_URL` defaults to `http://127.0.0.1:8986` and `OCE_API_KEY`
defaults to `sk-opencontextengine`. `status` is local-only and does not require
an API key. `observe` and `remove`
stage explicit editor changes in SQLite; run `sync` to publish them. Add
`--json` to `sync`, `status`, `retrieve`, `observe`, or `remove` for
machine-readable output. Global options such as `--root` must
appear before the subcommand.

## MCP

Install the optional MCP extra and expose the stdio server to an MCP host:

```powershell
uv sync --extra mcp
uv run oce-client-mcp --workspace C:\path\to\workspace
```

The server exposes one tool, `codebase-retrieval`. Workspace indexing belongs
to the MCP process rather than the coding agent: the server starts the initial
index in the background, watches the filesystem, and synchronizes only changed
paths. Unchanged files are identified by stored filesystem metadata and are not
read or rehashed on restart.

Declare each allowed workspace with a repeated `--workspace` argument. With one
workspace, the tool's `workspace_folder` input is optional. With multiple
workspaces it is required and must exactly match an allowed path. Other paths
are rejected.

```powershell
oce-client-mcp `
  --workspace C:\src\project-a `
  --workspace C:\src\project-b `
  --state-dir $env:LOCALAPPDATA\oce-client `
  --initial-sync background `
  --debounce-ms 500 `
  --ready-timeout 3
```

`--initial-sync` accepts `background` (default), `blocking`, or `off`; `off`
defers initialization until the first retrieval call. A tool call waits up to
`--ready-timeout` seconds for the latest observed filesystem generation. Its
result status is `ready`, `indexing`, or `error`; only a `ready` result contains
retrieval context. `OCE_API_URL`, `OCE_API_KEY`, `OCE_STATE_DIR`,
`OCE_DEBOUNCE_MS`, `OCE_INITIAL_SYNC`, `OCE_READY_TIMEOUT`, and `OCE_LOG_LEVEL`
provide environment equivalents. Keep the API key in the environment rather
than command arguments.

The server uses the same SQLite state and environment variables as the CLI. A
Codex-ready skill with the host configuration and command guidance is included
at `skills/oce-client/SKILL.md`.

After installing a wheel, locate or install that skill with:

```powershell
uv run oce-client skill path
uv run oce-client skill install
```

The default installation target is `$CODEX_HOME/skills/oce-client` or
`$HOME/.codex/skills/oce-client`. Existing skill directories are preserved;
pass `--force` only when intentionally updating one.

Keep `OCE_API_KEY` in the host's environment or secret manager; do not commit it
to an MCP configuration file.
