# Security Policy

## Reporting a vulnerability

Do not publish API keys, request bodies, database files, or exploit details in a public issue. Use GitHub private vulnerability reporting when enabled, or contact the repository maintainer through a private channel listed on their profile.

## Sensitive local data

LLM Gateway Desktop stores upstream credentials in its local SQLite-backed configuration. Treat the following as secrets:

- `~/.llm-gateway-desktop/llm-gateway.db`
- database backups
- exported configurations
- debug logs captured while request-body logging is enabled

The gateway should listen on `127.0.0.1` by default. Binding to a LAN or public interface can expose paid upstream credentials and model access. Use a strong local gateway key and an authenticated reverse proxy when remote access is necessary.

## Supported versions

Security fixes are currently provided only for the newest release. The project is early alpha and has not completed an independent security audit.
