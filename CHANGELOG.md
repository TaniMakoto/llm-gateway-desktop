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

### Known limitations

- Early alpha; full cross-platform builds and real-provider compatibility require additional validation
- Provider-specific protocol features may not translate losslessly
- Windows packages are unsigned and macOS packages are not notarized
