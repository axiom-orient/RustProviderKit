#!/usr/bin/env python3
"""Snapshot Swift @Test contracts with stable body hashes."""
from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

TEST_ATTRIBUTE = re.compile(r'@Test(?:\(\s*"((?:[^"\\]|\\.)*)"[^)]*\))?')
TEST_FUNCTION = re.compile(r'\bfunc\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(')


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def balanced_function(text: str, start: int) -> str:
    opening = text.find("{", start)
    if opening < 0:
        raise ValueError("test function body has no opening brace")
    depth = 0
    state = "code"
    block_depth = 0
    i = opening
    while i < len(text):
        c = text[i]
        n = text[i + 1] if i + 1 < len(text) else ""
        if state == "line":
            if c == "\n": state = "code"
            i += 1; continue
        if state == "block":
            if c == "/" and n == "*": block_depth += 1; i += 2; continue
            if c == "*" and n == "/":
                block_depth -= 1; i += 2
                if block_depth == 0: state = "code"
                continue
            i += 1; continue
        if state == "string":
            if c == "\\": i += 2; continue
            if c == '"': state = "code"
            i += 1; continue
        if c == "/" and n == "/": state = "line"; i += 2; continue
        if c == "/" and n == "*": state = "block"; block_depth = 1; i += 2; continue
        if c == '"': state = "string"; i += 1; continue
        if c == "{": depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0: return text[start:i + 1]
        i += 1
    raise ValueError("test function body is unclosed")


def contracts(reference: Path) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    for path in sorted((reference / "Tests").rglob("*.swift")):
        text = path.read_text(encoding="utf-8")
        cursor = 0
        while True:
            attribute = TEST_ATTRIBUTE.search(text, cursor)
            if not attribute: break
            function = TEST_FUNCTION.search(text, attribute.end())
            if not function: raise ValueError(f"@Test without function in {path}")
            body = balanced_function(text, function.start())
            relative = path.relative_to(reference).as_posix()
            title = bytes(attribute.group(1) or "", "utf-8").decode("unicode_escape")
            result.append({
                "id": f"SWIFT-{len(result) + 1:03d}",
                "path": relative,
                "function": function.group(1),
                "title": title,
                "body_sha256": sha256(body.encode("utf-8")),
            })
            cursor = function.start() + len(body)
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("reference", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--archive-sha256", default=None)
    args = parser.parse_args()
    reference = args.reference.resolve()
    values = contracts(reference)
    if len(values) != 92:
        raise SystemExit(f"expected 92 Swift contracts, found {len(values)}")
    source_files = sorted((reference / "Sources").rglob("*.swift"))
    payload = {
        "schema_version": 1,
        "reference_name": "SEMIProviderKit",
        "reference_archive_sha256": args.archive_sha256,
        "source_file_count": len(source_files),
        "test_file_count": len(list((reference / "Tests").rglob("*.swift"))),
        "contract_count": len(values),
        "contracts": values,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"PASS: snapshotted {len(values)} Swift contracts -> {args.output}")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
