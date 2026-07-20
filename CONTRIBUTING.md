# Contributing

Thanks for helping improve LLM Gateway Desktop.

## Before opening a pull request

1. Open an issue for large behavior or architecture changes.
2. Do not commit real API keys, database files, logs, or provider responses containing private data.
3. Keep protocol changes covered by focused unit tests or mock-server fixtures.
4. Preserve third-party copyright and license notices.

## Checks

```bash
pnpm install --frozen-lockfile
pnpm typecheck
pnpm format:check
cargo test --locked --manifest-path src-tauri/Cargo.toml gateway --lib
```

For user-facing changes, also test at least one non-streaming request, one SSE request, and one tool call through `tools/mock_upstream.py`.

## Commit scope

Keep pull requests focused. Avoid combining broad formatting changes with protocol behavior changes, because the translation code is difficult to review safely.
