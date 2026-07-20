# Source Validation Report

Project: LLM Gateway Desktop 0.1.0 alpha  
Validation date: 2026-07-20

## Completed in this environment

- JSON and TOML manifests parse successfully.
- The renamed Cargo root package is present in `Cargo.lock`.
- Required public routes are present.
- Python development tools compile successfully.
- Delimiter/static structure checks pass for the selected Rust files.
- Mock API tests pass 8/8 for health, model listing, JSON responses, and SSE streams.
- Public-source scan finds no private credentials. Example-looking AWS values are standard unit-test fixtures.
- Application branding, package names, data-directory names, icons, and release labels were replaced.
- The only intentional upstream project reference is the legally required attribution in `THIRD_PARTY_NOTICES.md`.

## Not executed here

- `cargo test`, `cargo check`, and release builds: Rust is not installed in this environment.
- Full `pnpm typecheck`: project dependencies are not installed.
- Real-provider tests: no credentials were requested or used.

## Reproduce the lightweight checks

```bash
python tools/static_check.py
python tools/mock_upstream.py --host 127.0.0.1 --port 18088
python tools/smoke_test.py \
  --base-url http://127.0.0.1:18088 \
  --api-key test-local-key \
  --model mock-chat
```

Use `.github/workflows/build-release.yml` for complete cross-platform compilation and packaging.
