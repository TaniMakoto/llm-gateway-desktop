# Public Release Audit

Date: 2026-07-20

## Rebranding completed

- Product name: `LLM Gateway Desktop`
- npm package: `llm-gateway-desktop`
- Rust package: `llm-gateway-desktop`
- Rust library crate: `llm_gateway_desktop_lib`
- Tauri identifier: `app.llmgateway.desktop`
- Data directory: `~/.llm-gateway-desktop`
- Window, tray, installer, release, and documentation labels updated
- Original application and tray icons replaced with new project-owned artwork
- Legacy brand strings removed from source, tests, comments, fixture identifiers, filenames, and workflow metadata

## Required attribution retained

The root `LICENSE` preserves the original MIT copyright and permission notice. `THIRD_PARTY_NOTICES.md` identifies the upstream project and derived areas. These files must remain in public source and redistributed packages.

## Validation completed

- Public-branding scan: passed
- JSON/TOML parsing: passed
- Cargo root package consistency: passed
- Python source compilation: passed
- Selected Rust delimiter checks: passed
- TypeScript syntax check: passed
- Mock protocol smoke tests: 8/8 passed
- Icon formats: PNG, ICO, and ICNS regenerated successfully

## Still required before a stable release

- Complete GitHub Actions build on Windows, Linux, and both macOS architectures
- `cargo check`, `cargo test`, and full frontend type checking
- Real-provider tests with redacted logs
- Windows signing and macOS notarization for warning-free distribution
- Independent security review before production use
