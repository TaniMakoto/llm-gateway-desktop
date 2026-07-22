# Changelog

All notable changes will be documented in this file.

## [0.1.0] - Unreleased

### Added

- Local multi-provider LLM API gateway
- OpenAI Chat, OpenAI Responses, and Anthropic Messages endpoints
- Cross-protocol request and response translation
- Model aliases and ordered failover routes
- Desktop management UI and system tray
- Cross-platform GitHub Actions packaging
- Provider-level custom User-Agent support
- OpenAI- and Anthropic-style upstream model discovery
- Cached model suggestions in the route editor

### Fixed

- Dashboard upstream routing status now distinguishes active requests from the last successful upstream
- Concurrent requests are aggregated by actual upstream, with per-upstream request counts available on hover
- Renaming a provider now refreshes the dashboard status from its stable provider ID instead of retaining the old name

### Known limitations

- Early alpha; full cross-platform builds and real-provider compatibility require additional validation
- Provider-specific protocol features may not translate losslessly
- Windows packages are unsigned and macOS packages are not notarized
