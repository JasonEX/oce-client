# Changelog

This project follows [Semantic Versioning](https://semver.org/).
Entries are generated from Conventional Commits.

## [Unreleased]

## [0.1.2] - 2026-09-02

### Fixed

- **watch**: normalize filesystem event paths across macOS and Windows without following final symbolic links
- **watch**: ignore non-mutating access events so local state reads cannot trigger repeated workspace synchronization
- **ci**: pin the setup-uv action to a published release so benchmark validation runs reliably

## [0.1.1] - 2026-09-02

### Added

- **benchmark**: add a pinned 50-case retrieval harness and diagnostic recipes, including an intent-classifier A/B pair
- **release**: build native client archives for Linux, Windows, and macOS

### Fixed

- **admission**: exclude common secret files and symbolic links from workspace uploads
- **state**: recover expired checkpoints without losing local generation state
- **watch**: report filesystem watcher failures and discard retrieval completed against an older generation
- **release**: preserve packaged archives when collecting build artifacts

### Changed

- **client**: replace the Python runtime and separate MCP entry point with one Rust binary
- **mcp**: describe semantic retrieval as broad discovery whose results require verification
- **benchmark**: keep Python only as optional development tooling that invokes the Rust client and records raw outcomes

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
