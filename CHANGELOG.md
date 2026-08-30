# Changelog

This project follows [Semantic Versioning](https://semver.org/).
Entries are generated from Conventional Commits.

## [Unreleased]

### Added

- Add per-workspace background indexing, incremental filesystem synchronization,
  workspace allowlists, and MCP runtime configuration.

### Changed

- Make code retrieval wait for the latest observed index generation and return
  explicit `ready`, `indexing`, or `error` status.
- Unify CLI and standalone MCP configuration loading with explicit workspace
  selection, documented argument/environment precedence, and environment-only
  API key loading.

### Removed

- Remove unsupported project overview and retrieval-paths service endpoints.
