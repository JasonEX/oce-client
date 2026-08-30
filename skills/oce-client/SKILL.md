---
name: oce-client
description: "Use the oce-client CLI to synchronize one local workspace and retrieve current code context from OpenContextEngine."
---

# OpenContextEngine CLI

This skill documents the `oce-client` command-line interface for an AI agent.
It is a CLI workflow: invoke a command, read its result, and continue the task.

## 1. Command Reference

### Version and package identity

- Distribution package: `opencontextengine-client`
- Python package: `oce_client`
- CLI executable: `oce-client`
- Current package version: `0.1.0`
- Check the installed CLI version with `oce-client --version`.

### Global options

Global options must appear before the subcommand:

```text
oce-client --root <workspace> --api-url <url> --state-path <file> <command>
```

- `--root`: workspace directory; otherwise use `OCE_WORKSPACE`, then the
  current directory.
- `--api-url`: service URL; otherwise use `OCE_API_URL`, then
  `http://127.0.0.1:8986`.
- `--state-path`: SQLite state file; otherwise use `OCE_STATE_PATH`, then
  `<workspace>/.oce-client/state.sqlite3`.
- `--ignore PATTERN`: add a runtime ignore pattern; repeat it when needed.

The API key has no CLI option. Load it through `OCE_API_KEY`; the local default
is `sk-opencontextengine`.

### Workspace commands

```text
oce-client --root <workspace> sync [--json]
oce-client --root <workspace> status [--json]
oce-client --root <workspace> retrieve [--scope workspace|working_set] [--json] <query>
oce-client --root <workspace> observe <path> [--content <text> | --file <file>] [--json]
oce-client --root <workspace> remove <path> [--json]
oce-client --root <workspace> watch [--debounce-ms <milliseconds>]
oce-client skill path [--json]
oce-client skill install [--target <directory>] [--force] [--json]
```

- `sync` scans the workspace, uploads new or changed blobs, and commits a new
  server checkpoint. This is the command that makes retrieval reflect the
  current files.
- `status` reads only the local SQLite inventory and checkpoint. It does not
  contact the service and does not require an API key.
- `retrieve` sends a natural-language code question to the service using the
  last local checkpoint. Use `--scope` only when the task requires the
  corresponding retrieval scope.
- `observe` stages explicit editor content in the local state. It does not
  publish the content until `sync` runs.
- `remove` stages a workspace-relative deletion. It also requires `sync` to
  publish the change.
- `watch` keeps a foreground process alive and incrementally syncs filesystem
  changes. Run an initial successful `sync` before starting it.

`skill path` and `skill install` are installation maintenance commands; they
are not part of normal code retrieval.

## 2. Workflow

### One-shot retrieval

Use this workflow when no long-running watcher is already maintaining the
workspace:

```text
1. Set OCE_WORKSPACE, OCE_API_URL, and OCE_API_KEY in the process environment.
2. Run `oce-client --root <workspace> sync --json`.
3. Run `oce-client --root <workspace> retrieve --json "<natural-language question>"`.
4. Parse JSON from stdout and use `formatted_retrieval` as code context.
```

Run `sync` again before a later retrieval when files may have changed. The
client persists inventory and checkpoint state, so unchanged files are not
uploaded again.

### Session retrieval with a watcher

For several questions against the same workspace:

```text
1. Run one successful `sync`.
2. Start `oce-client --root <workspace> watch` and keep it running.
3. Invoke `retrieve --json` for each AI question while the watcher is alive.
4. Stop the watcher when the workspace session ends.
```

The watcher handles changed paths incrementally. If it is stopped, return to
the one-shot workflow and run `sync` before retrieving.

### Unsaved editor state

When the agent has content that is not yet written to disk:

```text
oce-client --root <workspace> observe src/example.py --content "<text>"
oce-client --root <workspace> sync --json
oce-client --root <workspace> retrieve --json "<question>"
```

Use `remove` followed by `sync` for an unsaved deletion.

## 3. Important Notes

- This CLI handles one workspace per invocation. Pass the workspace explicitly
  when the agent knows it; do not accidentally index the agent's own process
  directory.
- Keep `OCE_API_KEY` in the environment or a secret manager. Never put the key
  in prompts, command arguments, logs, JSON output, or committed files.
- Use `--json` whenever another program or agent will consume the result.
  Treat stdout as the data channel and stderr as diagnostics.
- A successful `status` only proves that local state exists; it does not prove
  that the files on disk have been synchronized. Do not claim that retrieval is
  current until `sync` has succeeded or an active `watch` has processed the
  changes.
- `sync` and `retrieve` can take time on a first run or after a large change.
  Do not retry them concurrently against the same state file.
- Use only the commands and options documented above. In particular, do not
  invent a background or initial-sync option for this one-shot CLI.
- Retrieval describes the current code on disk and the selected checkpoint. It
  has no version-control history or knowledge of previous commits.
- Quote workspace paths and natural-language queries. Use workspace-relative
  paths with `observe` and `remove`.
- A zero exit code means the command completed successfully. A non-zero exit
  code means the result should not be treated as valid context; inspect stderr.
