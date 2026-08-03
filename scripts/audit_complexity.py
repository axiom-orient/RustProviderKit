#!/usr/bin/env python3
"""Deterministic source-level complexity and quadratic-risk audit.

This script deliberately does not treat line count as a defect. It inventories every
Rust file, finds large functions and loop nesting, rejects known front-shift patterns,
and requires the human-reviewed COMPLEXITY_AUDIT.md to account for every file.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

FUNCTION_RE = re.compile(
    r"\b(?:pub(?:\s*\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b"
)
WORD_OR_BRACE_RE = re.compile(r"\b(?:for|while|loop)\b|[{}]")
SCAN_IN_LOOP_RE = re.compile(
    r"\.(?:contains|position|find|any|all)\s*\(|\.iter\s*\(\)|\.into_iter\s*\(\)"
)
FRONT_SHIFT_RE = re.compile(r"\.(?:insert|remove)\s*\(\s*0\s*[,)]")


@dataclass(frozen=True)
class FileMetric:
    path: str
    lines: int
    functions: int
    max_function_lines: int
    max_function: str
    loops: int
    max_loop_nesting: int
    scans_inside_loops: int
    awaits: int
    locks: int
    sorts: int


def sanitize(text: str) -> str:
    """Replace comments and literals with spaces while preserving offsets/newlines."""
    out = list(text)
    i = 0
    state = "code"
    block_depth = 0
    raw_hashes = 0
    while i < len(text):
        c = text[i]
        n = text[i + 1] if i + 1 < len(text) else ""
        if state == "code":
            if c == "/" and n == "/":
                out[i] = out[i + 1] = " "
                i += 2
                state = "line"
                continue
            if c == "/" and n == "*":
                out[i] = out[i + 1] = " "
                i += 2
                state = "block"
                block_depth = 1
                continue
            if c == '"':
                out[i] = " "
                i += 1
                state = "string"
                continue
            if c == "'":
                # Lifetimes are left intact; only a plausible char literal is masked.
                end = i + 1
                if end < len(text) and text[end] == "\\":
                    end += 2
                else:
                    end += 1
                if end < len(text) and text[end] == "'":
                    for index in range(i, end + 1):
                        if text[index] != "\n":
                            out[index] = " "
                    i = end + 1
                    continue
            if c == "r":
                cursor = i + 1
                hashes = 0
                while cursor < len(text) and text[cursor] == "#":
                    hashes += 1
                    cursor += 1
                if cursor < len(text) and text[cursor] == '"':
                    for index in range(i, cursor + 1):
                        out[index] = " "
                    i = cursor + 1
                    state = "raw"
                    raw_hashes = hashes
                    continue
            i += 1
            continue
        if state == "line":
            if c == "\n":
                state = "code"
            else:
                out[i] = " "
            i += 1
            continue
        if state == "block":
            if c == "/" and n == "*":
                out[i] = out[i + 1] = " "
                block_depth += 1
                i += 2
                continue
            if c == "*" and n == "/":
                out[i] = out[i + 1] = " "
                block_depth -= 1
                i += 2
                if block_depth == 0:
                    state = "code"
                continue
            if c != "\n":
                out[i] = " "
            i += 1
            continue
        if state == "string":
            if c == "\\":
                out[i] = " "
                if i + 1 < len(text):
                    if text[i + 1] != "\n":
                        out[i + 1] = " "
                    i += 2
                else:
                    i += 1
                continue
            if c == '"':
                out[i] = " "
                i += 1
                state = "code"
                continue
            if c != "\n":
                out[i] = " "
            i += 1
            continue
        if state == "raw":
            if c == '"' and text.startswith("#" * raw_hashes, i + 1):
                out[i] = " "
                for index in range(i + 1, i + 1 + raw_hashes):
                    out[index] = " "
                i += 1 + raw_hashes
                state = "code"
                continue
            if c != "\n":
                out[i] = " "
            i += 1
            continue
    return "".join(out)


def line_number(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def matching_brace(code: str, opening: int) -> int | None:
    depth = 0
    for index in range(opening, len(code)):
        if code[index] == "{":
            depth += 1
        elif code[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    return None


def function_spans(code: str) -> list[tuple[str, int, int]]:
    spans: list[tuple[str, int, int]] = []
    for match in FUNCTION_RE.finditer(code):
        opening = code.find("{", match.end())
        semicolon = code.find(";", match.end())
        if opening < 0 or (semicolon >= 0 and semicolon < opening):
            continue
        closing = matching_brace(code, opening)
        if closing is None:
            continue
        spans.append((match.group("name"), opening, closing))
    return spans


def loop_metrics(body: str) -> tuple[int, int, int]:
    stack: list[bool] = []
    pending_loop = False
    loops = 0
    maximum = 0
    scans = 0
    last = 0
    for token in WORD_OR_BRACE_RE.finditer(body):
        segment = body[last : token.start()]
        if any(stack) and SCAN_IN_LOOP_RE.search(segment):
            scans += len(SCAN_IN_LOOP_RE.findall(segment))
        value = token.group(0)
        if value in {"for", "while", "loop"}:
            if value == "for" and body[token.end() :].lstrip().startswith("<"):
                last = token.end()
                continue
            pending_loop = True
            loops += 1
        elif value == "{":
            stack.append(pending_loop)
            pending_loop = False
            maximum = max(maximum, sum(stack))
        elif value == "}":
            if stack:
                stack.pop()
            pending_loop = False
        last = token.end()
    if any(stack) and SCAN_IN_LOOP_RE.search(body[last:]):
        scans += len(SCAN_IN_LOOP_RE.findall(body[last:]))
    return loops, maximum, scans


def metric(path: Path, root: Path) -> FileMetric:
    text = path.read_text(encoding="utf-8")
    code = sanitize(text)
    spans = function_spans(code)
    max_name = "-"
    max_lines = 0
    loops = nesting = scans = 0
    for name, opening, closing in spans:
        function_lines = line_number(code, closing) - line_number(code, opening) + 1
        if function_lines > max_lines:
            max_lines = function_lines
            max_name = name
        loop_count, loop_nesting, loop_scans = loop_metrics(code[opening : closing + 1])
        loops += loop_count
        nesting = max(nesting, loop_nesting)
        scans += loop_scans
    return FileMetric(
        path=path.relative_to(root).as_posix(),
        lines=len(text.splitlines()),
        functions=len(spans),
        max_function_lines=max_lines,
        max_function=max_name,
        loops=loops,
        max_loop_nesting=nesting,
        scans_inside_loops=scans,
        awaits=len(re.findall(r"\.await\b", code)),
        locks=len(re.findall(r"\.lock\s*\(", code)),
        sorts=len(re.findall(r"\.sort(?:_by|_by_key|_unstable|_unstable_by)?\s*\(", code)),
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("repository", type=Path)
    parser.add_argument("--json", type=Path)
    parser.add_argument("--skip-document-check", action="store_true")
    args = parser.parse_args()
    root = args.repository.resolve()
    files = sorted((root / "crates").rglob("*.rs"))
    errors: list[str] = []
    if not files:
        errors.append("no Rust source files found")
    metrics = [metric(path, root) for path in files]

    for path in files:
        code = sanitize(path.read_text(encoding="utf-8"))
        if FRONT_SHIFT_RE.search(code):
            errors.append(f"front-shifting collection operation remains: {path.relative_to(root)}")

    if not args.skip_document_check:
        audit_path = root / "COMPLEXITY_AUDIT.md"
        if not audit_path.is_file():
            errors.append("COMPLEXITY_AUDIT.md is missing")
        else:
            audit = audit_path.read_text(encoding="utf-8")
            for item in metrics:
                if f"`{item.path}`" not in audit:
                    errors.append(f"Rust file missing from complexity audit: {item.path}")

    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(
            json.dumps([asdict(item) for item in metrics], ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )

    total_lines = sum(item.lines for item in metrics)
    largest = max(metrics, key=lambda item: item.lines, default=None)
    longest = max(metrics, key=lambda item: item.max_function_lines, default=None)
    nested = [item for item in metrics if item.max_loop_nesting >= 2]
    print(f"Rust files: {len(metrics)}")
    print(f"Rust lines: {total_lines}")
    if largest:
        print(f"Largest file: {largest.path} ({largest.lines} lines)")
    if longest:
        print(
            f"Longest function: {longest.path}::{longest.max_function} "
            f"({longest.max_function_lines} lines)"
        )
    print(f"Files with nested loops: {len(nested)}")
    for item in nested:
        print(
            f"  nested={item.max_loop_nesting} scans={item.scans_inside_loops} "
            f"{item.path}"
        )

    if errors:
        for error in errors:
            print(f"FAIL: {error}", file=sys.stderr)
        return 1
    if args.skip_document_check:
        print("PASS: complexity inventory")
    else:
        print("PASS: complexity inventory and documented file coverage")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
