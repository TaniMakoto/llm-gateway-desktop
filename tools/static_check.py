#!/usr/bin/env python3
"""Dependency-free static checks that can run before installing project packages.

This does not replace `cargo test` or a Tauri build. It catches damaged JSON/TOML,
Python syntax errors, missing gateway files/routes, stale integration tests, and
obvious delimiter corruption in the Rust files modified for this fork.
"""

from __future__ import annotations

import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]

RUST_FILES = [
    "src-tauri/src/gateway.rs",
    "src-tauri/src/gateway_chat.rs",
    "src-tauri/src/lib.rs",
    "src-tauri/src/main.rs",
    "src-tauri/src/config.rs",
    "src-tauri/src/settings.rs",
    "src-tauri/src/panic_hook.rs",
    "src-tauri/src/database/mod.rs",
    "src-tauri/src/database/backup.rs",
    "src-tauri/src/proxy/server.rs",
    "src-tauri/src/proxy/types.rs",
    "src-tauri/src/proxy/forwarder.rs",
    "src-tauri/src/proxy/handlers.rs",
    "src-tauri/src/proxy/handler_context.rs",
    "src-tauri/src/proxy/model_mapper.rs",
    "src-tauri/src/proxy/response_processor.rs",
    "src-tauri/src/proxy/providers/codex.rs",
]


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)
    print(f"PASS  {message}")


def check_rust_delimiters(path: pathlib.Path) -> None:
    text = path.read_text(encoding="utf-8")
    stack: list[tuple[str, int]] = []
    pairs = {")": "(", "]": "[", "}": "{"}
    i = 0
    state = "code"
    block_depth = 0
    raw_hashes = 0

    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""

        if state == "line_comment":
            if ch == "\n":
                state = "code"
            i += 1
            continue
        if state == "block_comment":
            if ch == "/" and nxt == "*":
                block_depth += 1
                i += 2
            elif ch == "*" and nxt == "/":
                block_depth -= 1
                i += 2
                if block_depth == 0:
                    state = "code"
            else:
                i += 1
            continue
        if state == "string":
            if ch == "\\":
                i += 2
            elif ch == '"':
                state = "code"
                i += 1
            else:
                i += 1
            continue
        if state == "char":
            if ch == "\\":
                i += 2
            elif ch == "'":
                state = "code"
                i += 1
            else:
                i += 1
            continue
        if state == "raw":
            marker = '"' + ("#" * raw_hashes)
            if text.startswith(marker, i):
                state = "code"
                i += len(marker)
            else:
                i += 1
            continue

        if ch == "/" and nxt == "/":
            state = "line_comment"
            i += 2
        elif ch == "/" and nxt == "*":
            state = "block_comment"
            block_depth = 1
            i += 2
        elif ch == '"':
            state = "string"
            i += 1
        elif ch == "r":
            j = i + 1
            while j < len(text) and text[j] == "#":
                j += 1
            if j < len(text) and text[j] == '"':
                raw_hashes = j - (i + 1)
                state = "raw"
                i = j + 1
            else:
                i += 1
        elif ch == "'":
            # Rust lifetimes such as 'a and '_ are not character literals.
            if nxt == "_" or nxt.isalpha():
                closing = text.find("'", i + 1, min(i + 8, len(text)))
                if closing == -1:
                    i += 1
                else:
                    state = "char"
                    i += 1
            else:
                state = "char"
                i += 1
        elif ch in "([{":
            stack.append((ch, i))
            i += 1
        elif ch in ")]}" :
            if not stack:
                raise AssertionError(f"{path.name}: closing delimiter has no opener at byte {i}")
            opener, opener_pos = stack.pop()
            if opener != pairs[ch]:
                raise AssertionError(
                    f"{path.name}: mismatched delimiter {opener} at {opener_pos} and {ch} at {i}"
                )
            i += 1
        else:
            i += 1

    if state not in {"code", "line_comment"}:
        raise AssertionError(f"{path.name}: unterminated string/comment state {state}")
    if stack:
        raise AssertionError(f"{path.name}: unclosed delimiter {stack[-1]}")
    print(f"PASS  Rust delimiter structure {path.relative_to(ROOT)}")



def check_public_branding() -> None:
    forbidden = (
        "CC" + " Switch",
        "CC" + "-Switch",
        "cc" + "-switch",
        "cc" + "_switch",
        "cc" + "switch",
        "LLM Gateway" + " Switch",
        "llm-gateway" + "-switch",
        "llm_gateway" + "_switch",
    )
    allowed = {ROOT / "LICENSE", ROOT / "THIRD_PARTY_NOTICES.md"}
    text_suffixes = {
        ".rs", ".toml", ".json", ".md", ".yml", ".yaml", ".ts",
        ".tsx", ".js", ".cjs", ".css", ".html", ".py", ".txt",
        ".lock", ".plist", ".wxs", ".xml", ".sh", ".ps1",
    }
    matches: list[str] = []
    for path in ROOT.rglob("*"):
        if not path.is_file() or path in allowed:
            continue
        if path.suffix.lower() not in text_suffixes and path.name not in {
            ".gitignore", ".gitattributes", ".node-version"
        }:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        for token in forbidden:
            if token in text:
                matches.append(f"{path.relative_to(ROOT)}: {token}")
    check(not matches, "no legacy branding outside required license notices")


def main() -> None:
    check_public_branding()

    for rel in [
        "package.json",
        "tsconfig.json",
        "tsconfig.node.json",
        "src-tauri/tauri.conf.json",
        "src-tauri/tauri.windows.conf.json",
        "src-tauri/capabilities/default.json",
    ]:
        json.loads((ROOT / rel).read_text(encoding="utf-8"))
        print(f"PASS  parse {rel}")

    cargo = tomllib.loads((ROOT / "src-tauri/Cargo.toml").read_text(encoding="utf-8"))
    lock = tomllib.loads((ROOT / "src-tauri/Cargo.lock").read_text(encoding="utf-8"))
    roots = [
        package
        for package in lock["package"]
        if package["name"] == cargo["package"]["name"]
        and package["version"] == cargo["package"]["version"]
    ]
    check(len(roots) == 1, "Cargo.lock contains the renamed root package")

    for rel in ["tools/mock_upstream.py", "tools/smoke_test.py", "tools/static_check.py"]:
        source = (ROOT / rel).read_text(encoding="utf-8")
        compile(source, rel, "exec")
        print(f"PASS  Python syntax {rel}")

    server = (ROOT / "src-tauri/src/proxy/server.rs").read_text(encoding="utf-8")
    for route in [
        '"/health"',
        '"/v1/models"',
        '"/v1/messages"',
        '"/v1/chat/completions"',
        '"/v1/responses"',
        '"/v1/responses/compact"',
    ]:
        check(route in server, f"public route present: {route}")
    for removed in ['"/status"', '"/v1beta/models', '"/claude/v1/messages"']:
        check(removed not in server, f"legacy public route absent: {removed}")

    check(not (ROOT / "src-tauri/tests").exists(), "inherited integration tests removed")
    check((ROOT / "src-tauri/src/gateway.rs").exists(), "gateway configuration module exists")
    check((ROOT / "src-tauri/src/gateway_chat.rs").exists(), "Chat compatibility module exists")

    for rel in RUST_FILES:
        check_rust_delimiters(ROOT / rel)

    tsc = shutil.which("tsc")
    if tsc:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = pathlib.Path(temp_dir) / "tsconfig.json"
            config_path.write_text(
                json.dumps(
                    {
                        "compilerOptions": {
                            "target": "ES2022",
                            "lib": ["ES2022", "DOM", "DOM.Iterable"],
                            "module": "ESNext",
                            "moduleResolution": "Bundler",
                            "jsx": "react-jsx",
                            "noEmit": True,
                            "noCheck": True,
                            "noResolve": True,
                            "skipLibCheck": True,
                        },
                        "include": [
                            str(ROOT / "src/**/*.ts"),
                            str(ROOT / "src/**/*.tsx"),
                            str(ROOT / "vite.config.ts"),
                        ],
                    }
                ),
                encoding="utf-8",
            )
            subprocess.run([tsc, "-p", str(config_path), "--pretty", "false"], check=True)
            print("PASS  TypeScript/TSX syntax (tsc --noCheck)")
    else:
        print("SKIP  TypeScript syntax: global tsc is not installed")

    print("\nStatic checks passed. Run pnpm typecheck and cargo test/build for full validation.")


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1) from error
