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
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib
cargo test --locked --manifest-path src-tauri/Cargo.toml gateway::protocol_tests --lib -- --nocapture
```

For user-facing changes, also test at least one non-streaming request, one SSE request, and one tool call through `tools/mock_upstream.py`.

The inline `gateway::protocol_tests` suite starts the real gateway and local mock upstreams on ephemeral ports with in-memory databases. It covers all nine client/upstream protocol pairs in JSON and SSE, including tool calls and replay of the returned tool IDs. It also checks authentication, failover and 429 cooldown. It needs no external API credentials or desktop window. Do not substitute a direct mock-server smoke test for this suite.

## Commit scope

Keep pull requests focused. Avoid combining broad formatting changes with protocol behavior changes, because the translation code is difficult to review safely.

## Delivery workflow

When authorized to push and compile, complete the necessary local checks, push the
changes, and dispatch the appropriate GitHub build workflow. Report the commit,
workflow link, and actual validation results immediately, then end the task.
**Do not wait for or poll GitHub build/test completion**, unless the user explicitly
asks for the result. Distinguish checks that passed locally from tests delegated
to CI and builds that have only been triggered.

## Upstream failover

Before response output is committed, HTTP errors from one upstream (including
400, 401, 403, 413, and 422) remain eligible for the next available configured
candidate. Independent providers may implement the same public model differently.
Only return the final failure once eligible candidates are exhausted. A local
invalid request or local authentication failure can still stop immediately;
an already-started response stream must not be spliced with another provider.

When reviewing logs, distinguish an individual upstream rejection from the final
request outcome. `FWD-001` records candidate rotation; `FWD-002` records exhaustion.
