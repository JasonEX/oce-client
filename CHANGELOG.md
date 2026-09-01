# Changelog

This project follows [Semantic Versioning](https://semver.org/).
Entries are generated from Conventional Commits.

## [Unreleased]

### Fixed

- **admission**: exclude common secret files and symbolic links from workspace uploads
- **watch**: report filesystem watcher failures instead of silently continuing

### Changed

- **mcp**: describe semantic retrieval as broad discovery whose results require verification

## [0.1.0] - 2026-08-30

### Added

- **mcp**: add background incremental indexing
- **mcp**: expose unified codebase retrieval tool
- initialize oce-client

### Changed

- **skill**: keep interface guidance CLI-only
- **skill**: clarify CLI agent workflow
- **cli**: keep MCP as standalone entry point
- **config**: unify cli and mcp settings
- **client**: remove unsupported retrieval endpoints
