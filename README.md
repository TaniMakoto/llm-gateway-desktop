# LLM Gateway Desktop

A local desktop gateway for managing multiple LLM API providers behind one stable endpoint. It accepts OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages requests, then routes and translates them to a configured upstream provider.

> **Project status:** early alpha. The source includes automated validation and cross-platform packaging workflows, but real-provider compatibility still needs broader testing. Do not rely on it as the only path to a production API.

## Features

- Local endpoint, defaulting to `http://127.0.0.1:10888`
- Desktop configuration UI and system tray
- Local API-key authentication
- Multiple upstream providers, custom User-Agent, and custom headers
- Upstream model discovery with cached model selection in route editing
- Model aliases and ordered failover routes
- OpenAI Chat Completions compatible endpoint
- OpenAI Responses compatible endpoint
- Anthropic Messages compatible endpoint
- JSON and SSE streaming translation
- Standard tool-call and tool-result conversion
- SQLite-backed local configuration
- Windows, Linux, and macOS packaging with GitHub Actions

## Supported endpoints

```text
GET  /health
GET  /v1/models
POST /v1/chat/completions
POST /v1/responses
POST /v1/responses/compact
POST /v1/messages
```

The three client formats can be routed to OpenAI Chat, OpenAI Responses, or Anthropic Messages upstreams. Protocols are not perfectly equivalent, so provider-specific fields can be ignored, downgraded, or rejected by the upstream.

## Quick start

1. Open **Upstream Providers** and add an API base URL, API key, upstream format, authentication mode, and optional compatibility headers. Use **Fetch Models** to cache the upstream model list when the provider exposes a models endpoint.
2. Open **Model Routes** and create a local alias such as `best-code`. Add one or more ordered upstream targets.
3. Open **Gateway Settings**, save the listening address and local access key, then start the gateway.
4. Point a compatible client at the local endpoint.

OpenAI-compatible client settings:

```text
Base URL: http://127.0.0.1:10888/v1
API Key:  local key shown in Gateway Settings
Model:    your local model alias
```

Anthropic-compatible client settings:

```text
Base URL: http://127.0.0.1:10888
API Key:  local key shown in Gateway Settings
Model:    your local model alias
```

### OpenAI Chat example

```bash
curl http://127.0.0.1:10888/v1/chat/completions \
  -H "Authorization: Bearer local-sk-REPLACE_ME" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "best-code",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": false
  }'
```

### Anthropic Messages example

```bash
curl http://127.0.0.1:10888/v1/messages \
  -H "x-api-key: local-sk-REPLACE_ME" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "best-code",
    "max_tokens": 256,
    "messages": [{"role": "user", "content": "hello"}],
    "stream": false
  }'
```

## Data and security

Configuration and upstream credentials are stored locally under:

```text
~/.llm-gateway-desktop/
├── llm-gateway.db
├── settings.json
├── backups/
└── logs/
```

Provider compatibility settings are ordinary public HTTP options: a custom User-Agent, custom request headers, and an optional exact model-list URL. The project does not inject proprietary client tokens, device identifiers, private prompts, or other material intended to bypass an upstream access-control policy.

The listener defaults to loopback only. Keep it on `127.0.0.1` unless remote access is intentionally secured. Prompt bodies are not intended to be logged by default, but logs and database backups should still be treated as sensitive. See [SECURITY.md](SECURITY.md).

## Local development

Requirements:

- Node.js from `.node-version`
- pnpm 10
- Rust toolchain from `rust-toolchain.toml`
- Tauri 2 platform prerequisites

```bash
pnpm install --frozen-lockfile
pnpm typecheck
pnpm tauri dev
```

Run the dependency-free mock upstream:

```bash
python tools/mock_upstream.py
```

After configuring a route to the mock server, run:

```bash
python tools/smoke_test.py \
  --base-url http://127.0.0.1:10888 \
  --api-key local-sk-REPLACE_ME \
  --model best-code
```

## Building and releasing

The workflow at `.github/workflows/build-release.yml` validates the source and builds:

- Windows x64: NSIS and MSI
- Linux x64: AppImage and Debian package
- macOS Apple Silicon: app and DMG
- macOS Intel: app and DMG

A manual workflow run uploads build artifacts. Pushing a tag such as `v0.1.0` also creates a draft GitHub Release. See [docs/GITHUB_ACTIONS_BUILD.md](docs/GITHUB_ACTIONS_BUILD.md). A complete public-repository checklist is in [docs/PUBLISH_GITHUB.md](docs/PUBLISH_GITHUB.md). Provider compatibility and model discovery are documented in [docs/UPSTREAM_COMPATIBILITY.md](docs/UPSTREAM_COMPATIBILITY.md).

## Project structure

- `src/` — React desktop interface
- `src-tauri/src/gateway.rs` — gateway configuration and route materialization
- `src-tauri/src/gateway_chat.rs` — OpenAI Chat compatibility bridge
- `src-tauri/src/proxy/` — protocol translation, streaming, forwarding, and failover
- `tools/` — mock upstream and smoke tests

## Contributing

Issues and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting large changes.

## License

MIT. This repository contains third-party MIT-licensed code; required attribution is preserved in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
## Windows portable package

Release builds include a Windows x64 portable ZIP in addition to NSIS and MSI installers.
Extract the entire folder and run `LLM Gateway Desktop.exe`. The included
`portable.flag` enables portable mode, so application-owned data is written to
`data/` beside the executable. Keep the folder writable and do not run the EXE
directly from inside the ZIP archive.

Removing `portable.flag` makes the same executable use the normal per-user data
directory on its next launch.