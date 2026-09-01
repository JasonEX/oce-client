---
name: oce-client
description: "Synchronize an allowed workspace and retrieve current code context from OpenContextEngine with the oce-client binary."
---

# OpenContextEngine client

Use `oce-client` when a task needs semantic, architectural, cross-file, or
unknown-location code retrieval. Use native file or text search for a known path or
exact identifier.

## One-shot workflow

Set `OCE_API_URL`, `OCE_API_KEY`, and the workspace in the process environment, then
run:

```text
oce-client --root <workspace> sync --json
oce-client --root <workspace> retrieve <question> --json
```

Consume `formatted_retrieval` only when both commands exit successfully. Run `sync`
again before later retrieval when files may have changed.

## Long-running MCP workflow

Configure the same binary as an MCP stdio process:

```text
oce-client mcp --workspace <workspace>
```

The MCP tool is named `codebase-retrieval`. With one allowed workspace,
`workspace_folder` is optional. With repeated `--workspace` arguments it is required
and must exactly match an allowed root.

The tool returns one of:

- `ready` — use `formatted_retrieval`.
- `indexing` — wait briefly and retry; do not use older context.
- `error` — report the error and do not treat retrieval as valid.

## Explicit editor state

For unsaved text or an unsaved deletion:

```text
oce-client --root <workspace> observe <relative-path> --content <text>
oce-client --root <workspace> remove <relative-path>
oce-client --root <workspace> sync --json
```

Do not expose an API key in prompts, command arguments, logs, or committed files.
Quote paths and questions, use workspace-relative paths for `observe` and `remove`,
and never index a directory that the user did not place in scope.
