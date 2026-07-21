# Upstream compatibility and model discovery

Each provider can define the public HTTP compatibility settings used by normal inference requests and by model discovery:

- API format: OpenAI Chat, OpenAI Responses, or Anthropic Messages
- Authentication: automatic, Bearer, or `x-api-key`
- Custom User-Agent
- Custom request headers
- Codex client identity (optional)
- Optional exact model-list URL

## Codex client identity

Some upstreams that emulate the OpenAI Codex/ChatGPT backend validate a client fingerprint and reject requests from generic clients (for example returning `unauthorized client detected`). When the **impersonate Codex client** toggle is enabled for a provider, forwarded requests carry the public Codex CLI compatibility markers:

- `User-Agent: codex_cli_rs/<version>` (only when no custom User-Agent is set; a custom User-Agent takes precedence)
- `originator: codex_cli_rs` and a matching `version: <version>` header (sent as a pair)

The version defaults to a built-in value and can be overridden per provider when an upstream pins a specific client version. These are public compatibility identifiers only — no private tokens, identity prompts, or device fingerprints are sent. Use this only with upstreams you are authorized to access.

## Model discovery

When the model-list URL is empty, the application derives common candidates from the provider Base URL, normally `/v1/models` or `/models` when the Base URL already ends in a version segment.

OpenAI-style providers are queried with `Authorization: Bearer ...`. Anthropic-style providers in automatic authentication mode are queried with `x-api-key` and `anthropic-version: 2023-06-01`. The parser accepts both OpenAI and Anthropic model-list response shapes.

Fetched models are cached in the local provider configuration and appear as suggestions in the model-route editor. The local public `GET /v1/models` endpoint continues to expose local route aliases, because clients must call aliases rather than bypass the configured routing policy.

## Compatibility boundary

Custom User-Agent and headers are provided for documented or authorized provider compatibility. The application does not ship presets that copy proprietary client fingerprints, private identity prompts, device identifiers, session tokens, or other material intended to bypass an upstream client allowlist. Use an upstream's documented API protocol and credentials, or obtain explicit authorization from that upstream.
