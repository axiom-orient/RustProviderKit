#!/usr/bin/env python3
"""Validate the 92-row Swift→Rust parity ledger and optional live reference."""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

MATRIX_ROW = re.compile(r"^\|\s*(SWIFT-\d{3})\s*\|")
SWIFT_REFERENCE = re.compile(r"`(Tests/[^`]+\.swift)::([A-Za-z_][A-Za-z0-9_]*)`")
BODY_HASH = re.compile(r"`body:([0-9a-f]{12})`")
RUST_TEST = re.compile(r"`(?:platform|core|runtime|runtime-unit)::([A-Za-z_][A-Za-z0-9_]*)`")
TEST_FUNCTION = re.compile(
    r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def matrix_rows(path: Path) -> list[list[str]]:
    result: list[list[str]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if MATRIX_ROW.match(line):
            result.append([column.strip() for column in line.strip().strip("|").split("|")])
    return result


def rust_tests(repository: Path) -> set[str]:
    result: set[str] = set()
    for path in repository.glob("crates/**/*.rs"):
        result.update(TEST_FUNCTION.findall(path.read_text(encoding="utf-8")))
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("repository", type=Path)
    parser.add_argument("swift_reference", type=Path, nargs="?")
    args = parser.parse_args()
    repository = args.repository.resolve()
    snapshot_path = repository / "docs/SWIFT_CONTRACTS.json"
    matrix_path = repository / "FEATURE_MATRIX.md"
    errors: list[str] = []

    if not snapshot_path.is_file() or not matrix_path.is_file():
        print("FAIL: matrix or Swift contract snapshot is missing", file=sys.stderr)
        return 1
    snapshot = json.loads(snapshot_path.read_text(encoding="utf-8"))
    contracts = snapshot.get("contracts", [])
    rows = matrix_rows(matrix_path)
    if snapshot.get("contract_count") != 92 or len(contracts) != 92 or len(rows) != 92:
        errors.append(
            f"contract count mismatch snapshot={len(contracts)} rows={len(rows)} expected=92"
        )

    known_tests = rust_tests(repository)
    for index, (contract, row) in enumerate(zip(contracts, rows, strict=False), 1):
        expected_id = f"SWIFT-{index:03d}"
        if len(row) != 8:
            errors.append(f"{expected_id}: expected 8 columns, found {len(row)}")
            continue
        if row[0] != expected_id or contract.get("id") != expected_id:
            errors.append(f"{expected_id}: ID mismatch row={row[0]} snapshot={contract.get('id')}")
        expected_reference = (contract.get("path"), contract.get("function"))
        actual_reference = SWIFT_REFERENCE.search(row[2])
        if not actual_reference or actual_reference.groups() != expected_reference:
            errors.append(f"{expected_id}: Swift reference mismatch")
        body = BODY_HASH.search(row[2])
        expected_body = str(contract.get("body_sha256", ""))[:12]
        if not body or body.group(1) != expected_body:
            errors.append(f"{expected_id}: Swift body hash mismatch")
        if row[5] != "COMPLETE":
            errors.append(f"{expected_id}: source state is not COMPLETE: {row[5]}")
        if not row[6]:
            errors.append(f"{expected_id}: validation state is empty")
        if row[4].startswith("RUST_TEST:"):
            tests = RUST_TEST.findall(row[4])
            if not tests:
                errors.append(f"{expected_id}: RUST_TEST evidence has no test identifier")
            for test in tests:
                if test not in known_tests:
                    errors.append(f"{expected_id}: Rust test does not exist: {test}")
        elif not (row[4].startswith("SOURCE_TRACE:") or row[4].startswith("TYPE_SYSTEM:")):
            errors.append(f"{expected_id}: invalid parity evidence: {row[4]}")

    if args.swift_reference is not None:
        # Reuse the checked-in snapshot generator to compare full test body hashes.
        from snapshot_swift_contracts import contracts as extract_contracts
        actual = extract_contracts(args.swift_reference.resolve())
        comparable = [
            {key: value for key, value in item.items() if key in {"id", "path", "function", "title", "body_sha256"}}
            for item in contracts
        ]
        if actual != comparable:
            errors.append("checked-in Swift contract snapshot differs from the supplied reference")

    if errors:
        for error in errors:
            print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(
        f"PASS: Swift contracts={len(contracts)} matrix rows={len(rows)} "
        f"Rust tests={len(known_tests)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
