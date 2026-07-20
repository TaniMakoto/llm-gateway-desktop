#!/usr/bin/env python3
"""Dependency-free smoke tests for a running LLM Gateway Desktop instance."""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


@dataclass
class Result:
    name: str
    ok: bool
    detail: str


def request_json(url: str, headers: dict[str, str], body: dict[str, Any] | None = None) -> tuple[int, Any]:
    data = None if body is None else json.dumps(body).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method="GET" if body is None else "POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            raw = response.read().decode("utf-8")
            return response.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        try:
            parsed: Any = json.loads(raw)
        except json.JSONDecodeError:
            parsed = raw
        return exc.code, parsed


def request_sse(url: str, headers: dict[str, str], body: dict[str, Any]) -> tuple[int, list[str]]:
    req = urllib.request.Request(url, data=json.dumps(body).encode("utf-8"), headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            events: list[str] = []
            for raw_line in response:
                line = raw_line.decode("utf-8", errors="replace").strip()
                if line.startswith("data:"):
                    events.append(line[5:].strip())
            return response.status, events
    except urllib.error.HTTPError as exc:
        return exc.code, [exc.read().decode("utf-8", errors="replace")]


def contains_text(value: Any, needle: str) -> bool:
    if isinstance(value, str):
        return needle in value
    if isinstance(value, dict):
        return any(contains_text(item, needle) for item in value.values())
    if isinstance(value, list):
        return any(contains_text(item, needle) for item in value)
    return False


def run(args: argparse.Namespace) -> list[Result]:
    base = args.base_url.rstrip("/")
    openai_headers = {
        "Authorization": f"Bearer {args.api_key}",
        "Content-Type": "application/json",
    }
    anthropic_headers = {
        "x-api-key": args.api_key,
        "anthropic-version": "2023-06-01",
        "Content-Type": "application/json",
    }
    marker = "gateway-smoke-marker"
    results: list[Result] = []

    status, value = request_json(f"{base}/health", {})
    results.append(Result("health", status == 200, f"HTTP {status}: {value}"))

    status, value = request_json(f"{base}/v1/models", openai_headers)
    models_ok = status == 200 and contains_text(value, args.model)
    results.append(Result("models", models_ok, f"HTTP {status}: route alias {'found' if models_ok else 'not found'}"))

    chat_body = {
        "model": args.model,
        "messages": [{"role": "user", "content": marker}],
        "stream": False,
    }
    status, value = request_json(f"{base}/v1/chat/completions", openai_headers, chat_body)
    results.append(Result("chat-json", status == 200 and contains_text(value, marker), f"HTTP {status}: {value}"))

    responses_body = {"model": args.model, "input": marker, "stream": False}
    status, value = request_json(f"{base}/v1/responses", openai_headers, responses_body)
    results.append(Result("responses-json", status == 200 and contains_text(value, marker), f"HTTP {status}: {value}"))

    anthropic_body = {
        "model": args.model,
        "max_tokens": 128,
        "messages": [{"role": "user", "content": marker}],
        "stream": False,
    }
    status, value = request_json(f"{base}/v1/messages", anthropic_headers, anthropic_body)
    results.append(Result("anthropic-json", status == 200 and contains_text(value, marker), f"HTTP {status}: {value}"))

    for name, path, headers, body in (
        ("chat-sse", "/v1/chat/completions", openai_headers, {**chat_body, "stream": True}),
        ("responses-sse", "/v1/responses", openai_headers, {**responses_body, "stream": True}),
        ("anthropic-sse", "/v1/messages", anthropic_headers, {**anthropic_body, "stream": True}),
    ):
        status, events = request_sse(f"{base}{path}", headers, body)
        joined = "\n".join(events)
        results.append(Result(name, status == 200 and marker in joined, f"HTTP {status}: {len(events)} data events"))

    return results


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Smoke-test a running LLM Gateway Desktop")
    parser.add_argument("--base-url", default="http://127.0.0.1:10888")
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--model", required=True, help="Configured local route alias")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    try:
        results = run(args)
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        print(f"Connection failed: {exc}", file=sys.stderr)
        raise SystemExit(2) from exc

    width = max(len(result.name) for result in results)
    for result in results:
        print(f"{'PASS' if result.ok else 'FAIL'}  {result.name:<{width}}  {result.detail}")

    failed = [result for result in results if not result.ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} checks passed")
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
