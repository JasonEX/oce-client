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

### Removed

- Remove unsupported project overview and retrieval-paths service endpoints.
