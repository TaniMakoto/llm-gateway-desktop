# Upstream compatibility and model discovery

Each provider can define the public HTTP compatibility settings used by normal inference requests and by model discovery:

- API format: OpenAI Chat, OpenAI Responses, or Anthropic Messages
- Authentication: automatic, Bearer, or `x-api-key`
- Custom User-Agent
- Custom request headers
- Optional exact model-list URL

## Model discovery

When the model-list URL is empty, the application derives common candidates from the provider Base URL, normally `/v1/models` or `/models` when the Base URL already ends in a version segment.

OpenAI-style providers are queried with `Authorization: Bearer ...`. Anthropic-style providers in automatic authentication mode are queried with `x-api-key` and `anthropic-version: 2023-06-01`. The parser accepts both OpenAI and Anthropic model-list response shapes.

Fetched models are cached in the local provider configuration and appear as suggestions in the model-route editor. The local public `GET /v1/models` endpoint continues to expose local route aliases, because clients must call aliases rather than bypass the configured routing policy.

## Compatibility boundary

Custom User-Agent and headers are provided for documented or authorized provider compatibility. The application does not ship presets that copy proprietary client fingerprints, private identity prompts, device identifiers, session tokens, or other material intended to bypass an upstream client allowlist. Use an upstream's documented API protocol and credentials, or obtain explicit authorization from that upstream.
