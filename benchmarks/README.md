# OCE retrieval benchmark

This source-backed harness evaluates the same workspace sync and retrieval path used by
`oce-client`; it does not maintain a second uploader or bypass checkpoint semantics. The
checked-in corpus contains 50 queries across pinned public revisions of `oce` and
`oce-client`, covering symbol definition, feature location, cross-file flow, architecture,
configuration, references, and bug localization.

`variants.json` defines cumulative ablations from recursive dense retrieval through hybrid
recall, selection, and model reranking. A named run is accepted only when the server's
authenticated `/admin/index-stats` runtime profile matches every declared switch. This
prevents a result label from silently describing a configuration that was not running.
The same preflight requires a compatible persisted index fingerprint and records the
resolved embedding model and dimensions reported by the server.

The harness reports Top-1, Recall@10, MRR, nDCG@10, returned characters, and end-to-end
latency. Workspace-sync and query errors remain in the raw result and score as zero rather
than disappearing.
When `OCE_ADMIN_API_KEY` is available on an isolated benchmark server, it also records the
delta in external model calls and tokens after excluding workspace indexing. Optional
per-kind prices convert that delta into an explicit cost estimate. Agent task outcomes are
accepted as a separate boolean evidence file; retrieval scores never manufacture an
automatic task-success claim.

## Prepare pinned workspaces

From the `oce-client` repository:

```bash
uv run python benchmarks/evaluate.py validate
uv run python benchmarks/evaluate.py variants
uv run python benchmarks/evaluate.py prepare --workdir /tmp/oce-benchmark-workspaces
```

`prepare` only creates missing repository directories. Existing directories must already
be clean and at the required revision; the command will not reset or overwrite them.

## Run a variant

Inspect one checked-in configuration:

```bash
uv run python benchmarks/evaluate.py variants dense-exact-path
```

Apply all of its `environment` values to an isolated OCE server, use a fresh server data
directory, and start the service. A fresh index is required whenever chunking changes;
reusing a data directory would compare new retrieval controls against old chunks. Disable
unrelated traffic so monitoring deltas remain attributable to this run.

Then run the named variant:

```bash
export OCE_API_URL=http://127.0.0.1:8986
export OCE_API_KEY=sk-opencontextengine
export OCE_ADMIN_API_KEY=sk-admin

uv run python benchmarks/evaluate.py run \
  --workdir /tmp/oce-benchmark-workspaces \
  --variant dense-exact-path \
  --metadata embedding=Qwen3-Embedding-4B \
  --output /tmp/oce-results/dense-exact-path.json
```

`OCE_ADMIN_API_KEY` is mandatory for named variants: the harness checks the live embedding,
chunking, recall, priority, selector, rerank, rewrite, intent, decomposition, and query-cache
settings before any workspace sync. It also requires the server's persisted index profile
to be compatible, fingerprinted, and backed by a resolved embedding model/dimension. The
checked-in variants disable the query-vector cache so one run cannot benefit from warm
queries left by another.

The default six-second settling interval allows the asynchronous metrics buffer to flush
before snapshots. Pass prices in USD per million tokens when a cost estimate is wanted:

```bash
uv run python benchmarks/evaluate.py run \
  --workdir /tmp/oce-benchmark-workspaces \
  --variant adaptive-llm-rerank \
  --price embed=0.08 \
  --price rerank=0.10 \
  --price llm_rerank=0.20 \
  --output /tmp/oce-results/adaptive-rerank.json
```

`--label NAME` remains available for exploratory configurations that are not in
`variants.json`. Such runs deliberately skip live-profile verification and record
`variant: null`; do not mix them with verified ablations without documenting the exact
server environment in `--metadata`.

To attach agent-task evidence, provide a JSON object mapping case IDs to booleans:

```json
{
  "oce-flow-http-retrieval": true,
  "client-flow-sync": false
}
```

Use it with `--task-outcomes outcomes.json`. Missing outcomes remain `null` and are excluded
from the task-success denominator.

Filters such as `--repository oce`, `--category architecture`, and
`--case oce-symbol-retrieval-pipeline` can be repeated. The default state is temporary and
fresh for every run; `--state-dir` is available only when deliberate state reuse is part of
the experiment.

## Compare variants

Run each configuration against its own server session, client state, and server data
directory. Do not let an agent choose between retrieval systems inside one session; that
confounds attribution.

```bash
uv run python benchmarks/evaluate.py compare /tmp/oce-results/*.json
```

The command prints a Markdown table with retrieval quality, task-success evidence, latency,
returned context, external tokens, and estimated cost. Result JSON stores queries, expected
paths, retrieved paths, metrics, and errors, but never stores returned source-code content or
API keys.

The corpus is a checked-in starting point, not a claim that 50 cases cover every repository
shape. Add cases from real coding work with reviewed path labels, keep failures, and compare
variants on the identical pinned corpus before drawing quality conclusions.
