#!/usr/bin/env python3
"""Dependency-free mock LLM upstream for local gateway development.

Provides OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages
compatible endpoints on a single HTTP server. Both JSON and SSE responses are
supported. This server never contacts the network outside localhost.
"""

from __future__ import annotations

import argparse
import json
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import urlparse


def json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def read_text_from_content(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts: list[str] = []
    for item in content:
        if not isinstance(item, dict):
            continue
        kind = item.get("type")
        if kind in {"text", "input_text", "output_text"}:
            text = item.get("text")
            if isinstance(text, str):
                parts.append(text)
    return " ".join(parts)


def extract_prompt(payload: dict[str, Any]) -> str:
    messages = payload.get("messages")
    if isinstance(messages, list):
        for message in reversed(messages):
            if isinstance(message, dict) and message.get("role") == "user":
                text = read_text_from_content(message.get("content"))
                if text:
                    return text

    input_value = payload.get("input")
    if isinstance(input_value, str):
        return input_value
    if isinstance(input_value, list):
        for item in reversed(input_value):
            if not isinstance(item, dict):
                continue
            text = read_text_from_content(item.get("content"))
            if text:
                return text
    return "hello"


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "LLMGatewayMock/0.1"

    def log_message(self, fmt: str, *args: object) -> None:
        print(f"[{self.log_date_time_string()}] {self.address_string()} {fmt % args}")

    def do_GET(self) -> None:  # noqa: N802
        path = urlparse(self.path).path.rstrip("/") or "/"
        if path == "/health":
            self.send_json(200, {"status": "ok", "service": "mock-upstream"})
            return
        if path == "/v1/models":
            self.send_json(
                200,
                {
                    "object": "list",
                    "data": [
                        {"id": "mock-chat", "object": "model", "owned_by": "mock"},
                        {"id": "mock-responses", "object": "model", "owned_by": "mock"},
                        {"id": "mock-anthropic", "object": "model", "owned_by": "mock"},
                    ],
                },
            )
            return
        self.send_json(404, {"error": {"message": f"Unknown path: {path}"}})

    def do_POST(self) -> None:  # noqa: N802
        try:
            payload = self.read_json()
        except ValueError as exc:
            self.send_json(400, {"error": {"message": str(exc)}})
            return

        path = urlparse(self.path).path.rstrip("/")
        if payload.get("force_error") or self.headers.get("x-mock-force-error") == "1":
            self.send_json(503, {"error": {"message": "forced mock failure", "type": "mock_error"}})
            return

        delay_ms = payload.get("mock_delay_ms", 0)
        if isinstance(delay_ms, (int, float)) and delay_ms > 0:
            time.sleep(min(float(delay_ms) / 1000.0, 10.0))

        if path == "/v1/chat/completions":
            self.handle_chat(payload)
            return
        if path in {"/v1/responses", "/responses"}:
            self.handle_responses(payload)
            return
        if path in {"/v1/messages", "/messages"}:
            self.handle_anthropic(payload)
            return
        self.send_json(404, {"error": {"message": f"Unknown path: {path}"}})

    def read_json(self) -> dict[str, Any]:
        try:
            length = int(self.headers.get("content-length", "0"))
        except ValueError as exc:
            raise ValueError("Invalid Content-Length") from exc
        raw = self.rfile.read(length) if length else b"{}"
        try:
            value = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ValueError(f"Invalid JSON: {exc}") from exc
        if not isinstance(value, dict):
            raise ValueError("JSON request body must be an object")
        return value

    def send_json(self, status: int, value: Any) -> None:
        body = json_bytes(value)
        self.send_response(status)
        self.send_header("content-type", "application/json; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def begin_sse(self) -> None:
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()

    def write_sse(self, data: Any, event: str | None = None) -> None:
        if event:
            self.wfile.write(f"event: {event}\n".encode("utf-8"))
        encoded = data if isinstance(data, str) else json.dumps(data, ensure_ascii=False, separators=(",", ":"))
        self.wfile.write(f"data: {encoded}\n\n".encode("utf-8"))
        self.wfile.flush()

    def handle_chat(self, payload: dict[str, Any]) -> None:
        model = str(payload.get("model") or "mock-chat")
        prompt = extract_prompt(payload)
        text = f"mock chat reply: {prompt}"
        response_id = f"chatcmpl-{uuid.uuid4().hex[:16]}"
        created = int(time.time())

        if payload.get("stream"):
            self.begin_sse()
            first = {
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": None}],
            }
            second = {
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}],
            }
            final = {
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 5, "total_tokens": 9},
            }
            for item in (first, second, final):
                self.write_sse(item)
            self.write_sse("[DONE]")
            return

        self.send_json(
            200,
            {
                "id": response_id,
                "object": "chat.completion",
                "created": created,
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": text},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 4, "completion_tokens": 5, "total_tokens": 9},
            },
        )

    def handle_responses(self, payload: dict[str, Any]) -> None:
        model = str(payload.get("model") or "mock-responses")
        prompt = extract_prompt(payload)
        text = f"mock responses reply: {prompt}"
        response_id = f"resp_{uuid.uuid4().hex[:16]}"
        base = {
            "id": response_id,
            "object": "response",
            "created_at": int(time.time()),
            "status": "completed",
            "model": model,
            "output": [
                {
                    "id": f"msg_{uuid.uuid4().hex[:12]}",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                }
            ],
            "usage": {"input_tokens": 4, "output_tokens": 5, "total_tokens": 9},
        }

        if payload.get("stream"):
            self.begin_sse()
            created = dict(base)
            created["status"] = "in_progress"
            created["output"] = []
            self.write_sse({"type": "response.created", "response": created}, "response.created")
            item = base["output"][0]
            self.write_sse(
                {"type": "response.output_item.added", "output_index": 0, "item": {**item, "content": []}},
                "response.output_item.added",
            )
            self.write_sse(
                {
                    "type": "response.content_part.added",
                    "item_id": item["id"],
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                },
                "response.content_part.added",
            )
            self.write_sse(
                {
                    "type": "response.output_text.delta",
                    "item_id": item["id"],
                    "output_index": 0,
                    "content_index": 0,
                    "delta": text,
                },
                "response.output_text.delta",
            )
            self.write_sse(
                {
                    "type": "response.output_text.done",
                    "item_id": item["id"],
                    "output_index": 0,
                    "content_index": 0,
                    "text": text,
                },
                "response.output_text.done",
            )
            self.write_sse({"type": "response.completed", "response": base}, "response.completed")
            return

        self.send_json(200, base)

    def handle_anthropic(self, payload: dict[str, Any]) -> None:
        model = str(payload.get("model") or "mock-anthropic")
        prompt = extract_prompt(payload)
        text = f"mock anthropic reply: {prompt}"
        message_id = f"msg_{uuid.uuid4().hex[:16]}"
        message = {
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": 4, "output_tokens": 5},
        }

        if payload.get("stream"):
            self.begin_sse()
            self.write_sse(
                {
                    "type": "message_start",
                    "message": {**message, "content": [], "stop_reason": None, "usage": {"input_tokens": 4, "output_tokens": 0}},
                },
                "message_start",
            )
            self.write_sse(
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
                "content_block_start",
            )
            self.write_sse(
                {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}},
                "content_block_delta",
            )
            self.write_sse({"type": "content_block_stop", "index": 0}, "content_block_stop")
            self.write_sse(
                {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 5}},
                "message_delta",
            )
            self.write_sse({"type": "message_stop"}, "message_stop")
            return

        self.send_json(200, message)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a local mock LLM upstream")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", default=19090, type=int)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    server = ThreadingHTTPServer((args.host, args.port), MockHandler)
    print(f"Mock upstream listening at http://{args.host}:{args.port}")
    print("Endpoints: /v1/chat/completions, /v1/responses, /v1/messages, /health")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopping mock upstream")
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
