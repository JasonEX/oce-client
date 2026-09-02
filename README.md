# oce-client

`oce-client` is the standalone Rust client for OpenContextEngine. One binary owns
workspace admission, incremental synchronization, checkpoint recovery, retrieval,
filesystem watching, and the MCP stdio interface.

The maintained repository is <https://github.com/JasonEX/oce-client>. The original
`oce-ai/oce-client` repository is tracked only as Git upstream.

## Install

Download the archive for Windows, Linux, or macOS from
[GitHub Releases](https://github.com/JasonEX/oce-client/releases), extract
`oce-client` (`oce-client.exe` on Windows), and place it on `PATH`.

To build from source with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/JasonEX/oce-client --locked
```

Run `oce-client --version` to confirm the installed version.

## Configuration

The client reads these environment variables:

- `OCE_API_URL` — service URL; defaults to `http://127.0.0.1:8986`.
- `OCE_API_KEY` — bearer key; defaults to `sk-opencontextengine` for local use.
- `OCE_WORKSPACE` — one workspace root.
- `OCE_WORKSPACES` — platform-separated workspace roots for MCP.
- `OCE_STATE_PATH` — SQLite state file for one workspace.
- `OCE_STATE_DIR` — directory of per-workspace MCP state files.
- `OCE_IGNORE` — comma- or newline-separated runtime ignore patterns.
- `OCE_DEBOUNCE_MS`, `OCE_INITIAL_SYNC`, and `OCE_READY_TIMEOUT` — MCP lifecycle
  settings.

Keep production API keys in the environment or a secret manager.

## CLI

```sh
oce-client --root /path/to/project sync
oce-client --root /path/to/project status
oce-client --root /path/to/project retrieve "where is authentication handled?"
oce-client --root /path/to/project observe src/example.rs --content "unsaved text"
oce-client --root /path/to/project remove src/obsolete.rs
oce-client --root /path/to/project list-files
oce-client --root /path/to/project watch
```

`sync`, `status`, `list-files`, `retrieve`, `observe`, and `remove` accept `--json`.
Global `--root`, `--api-url`, `--state-path`, and repeated `--ignore` options may
appear before or after the subcommand. `status` and `list-files` are local-only;
`list-files` prints the files a sync would upload. Operations that contact OCE use
the configured bearer key.

The client stores state at `.oce-client/state-v1.sqlite3` by default and refuses to
open a state database written with a different schema version.

## MCP

Run MCP through the same binary:

```sh
oce-client mcp --workspace /path/to/project
```

An MCP host configuration can use:

```json
{
  "command": "/absolute/path/to/oce-client",
  "args": ["mcp", "--workspace", "/absolute/path/to/project"],
  "env": {
    "OCE_API_URL": "http://127.0.0.1:8986",
    "OCE_API_KEY": "sk-opencontextengine"
  }
}
```

Repeat `--workspace` for multiple allowed roots. With multiple workspaces, a tool
call must provide the matching `workspace_folder`. `--state-dir` gives each root its
own SQLite database. `--initial-sync` accepts `background`, `blocking`, or `off`;
`--ready-timeout` controls how long a tool call waits for the latest observed
filesystem generation.

The server exposes one tool, `codebase-retrieval`. Results are `ready`, `indexing`,
or `error`; only `ready` includes retrieval context. A result computed while the
workspace changes is discarded rather than returned as current context.

The bundled Codex skill can be materialized or installed with:

```sh
oce-client skill path
oce-client skill install
```

## File admission and state

Admission combines `.gitignore`, `.oceignore`, runtime patterns, and built-in cache,
dependency, and secret exclusions. Hard secret exclusions cannot be negated. Files
must be regular UTF-8 text no larger than 1 MiB. Symbolic links are not followed, and
the physical path is checked again around each read so a changed parent cannot escape
the workspace.

Explicit `observe` content is durable until removed or replaced. A successful sync
records the generation and server checkpoint in one local SQLite transaction after
the server succeeds; failed upload or checkpoint operations remain retryable.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

The Python code under `benchmarks/` is development tooling, not a second client
runtime. It invokes the Rust binary for synchronization, retrieval, and file
admission, so even the lexical baseline searches exactly the files the client would
upload:

```sh
uv sync --locked
uv run python benchmarks/evaluate.py validate
uv run python benchmarks/evaluate.py run --help
```

Tags matching `vX.Y.Z` build native archives for x86-64 Linux, x86-64 Windows,
x86-64 macOS, and Apple Silicon macOS on GitHub Actions.
