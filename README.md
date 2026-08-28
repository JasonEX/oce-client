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
uv run oce-client-mcp
```

The server exposes one tool, `codebase-retrieval`. Each call scans the selected
workspace, initializes local state when needed, synchronizes changed files, and
then retrieves current code context. It uses the same SQLite state and
environment variables as the CLI. A Codex-ready skill with the host
configuration and command guidance is included at `skills/oce-client/SKILL.md`.

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
