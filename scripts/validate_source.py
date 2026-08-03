#!/usr/bin/env python3
"""Fail-closed static repository validation that does not require a Rust toolchain."""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path, PurePosixPath

REQUIRED_CRATES = {
    "rust-provider-kit-core": "crates/rust-provider-kit-core",
    "rust-provider-kit-runtime": "crates/rust-provider-kit-runtime",
    "rust-provider-kit-platform": "crates/rust-provider-kit-platform",
}
REQUIRED_REPORTS = {
    "Cargo.lock",
    "README.md",
    "COMPLETION_REPORT.md",
    "FEATURE_MATRIX.md",
    "VALIDATION_REPORT.md",
    "VERIFY_LOCAL.md",
    "COMPLEXITY_AUDIT.md",
    "TRUNK_COMPARISON_REPORT.md",
    "docs/ARCHITECTURE.md",
    "docs/INTERFACE_CONTRACT.md",
    "docs/SWIFT_CONTRACTS.json",
}
FORBIDDEN_PRODUCTION = re.compile(
    r"\b(?:TODO|FIXME)\b|\b(?:todo|unimplemented|unreachable|panic)!\s*\(|\.unwrap\s*\(\s*\)|\.expect\s*\(|\bunsafe\b"
)
CACHE_PARTS = {"target", ".build", ".swiftpm", "DerivedData", "__pycache__", ".pytest_cache"}
IGNORED_ROOT_PARTS = {".git"}
ARCHIVE_SUFFIXES = (".zip", ".tar", ".tar.gz", ".tgz", ".tar.xz", ".7z")
OBSOLETE_PATHS = {
    "PARITY_REPORT.md",
    "crates/rust-provider-kit-runtime/tests/runtime_contracts.rs",
    "crates/rust-provider-kit-platform/tests/platform_contracts.rs",
}
EXPECTED_RUNTIME_EXPORTS = {
    "pub use in_memory_credential_store::InMemoryProviderCredentialStore;",
    "pub use openrouter_oauth::OpenRouterOAuthRegistrationRequest;",
    "pub use runtime::ProviderRuntime;",
}
EXPECTED_PLATFORM_EXPORTS = {
    "pub use loopback::{LoopbackAuthorizationSession, PreparedLoopbackAuthorization};",
    "pub use pkce::ProviderPkceGenerator;",
}


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def scan_rust(path: Path) -> list[str]:
    """Lexically validate comments, strings, chars, and delimiter balance."""
    text = path.read_text(encoding="utf-8")
    errors: list[str] = []
    stack: list[tuple[str, int]] = []
    pairs = {")": "(", "]": "[", "}": "{"}
    i = 0
    line = 1
    block_depth = 0
    state = "code"
    raw_hashes = 0
    while i < len(text):
        c = text[i]
        n = text[i + 1] if i + 1 < len(text) else ""
        if c == "\n":
            line += 1
            if state == "line_comment":
                state = "code"
            i += 1
            continue
        if state == "line_comment":
            i += 1
            continue
        if state == "block_comment":
            if c == "/" and n == "*":
                block_depth += 1
                i += 2
            elif c == "*" and n == "/":
                block_depth -= 1
                i += 2
                if block_depth == 0:
                    state = "code"
            else:
                i += 1
            continue
        if state in {"string", "byte_string"}:
            if c == "\\":
                i += 2
            elif c == '"':
                state = "code"
                i += 1
            else:
                i += 1
            continue
        if state in {"char", "byte_char"}:
            if c == "\\":
                i += 2
            elif c == "'":
                state = "code"
                i += 1
            else:
                i += 1
            continue
        if state == "raw_string":
            if c == '"' and text.startswith("#" * raw_hashes, i + 1):
                i += 1 + raw_hashes
                state = "code"
            else:
                i += 1
            continue

        # code
        if c == "/" and n == "/":
            state = "line_comment"
            i += 2
            continue
        if c == "/" and n == "*":
            state = "block_comment"
            block_depth = 1
            i += 2
            continue
        # raw strings: r"", r#""#, br#""#
        start = i
        if c == "b" and n == "r":
            start = i + 1
        if text[start:start + 1] == "r":
            j = start + 1
            while j < len(text) and text[j] == "#":
                j += 1
            if j < len(text) and text[j] == '"':
                raw_hashes = j - (start + 1)
                state = "raw_string"
                i = j + 1
                continue
        if c == "b" and n == '"':
            state = "byte_string"
            i += 2
            continue
        if c == "b" and n == "'":
            state = "byte_char"
            i += 2
            continue
        if c == '"':
            state = "string"
            i += 1
            continue
        # Lifetime syntax starts with apostrophe followed by identifier and is not a char.
        if c == "'":
            after = text[i + 1:i + 2]
            j = i + 1
            while j < len(text) and (text[j].isalnum() or text[j] == "_"):
                j += 1
            if after and (after.isalpha() or after == "_") and (j >= len(text) or text[j] != "'"):
                i = j
                continue
            state = "char"
            i += 1
            continue
        if c in "([{":
            stack.append((c, line))
        elif c in ")]}":
            if not stack or stack[-1][0] != pairs[c]:
                errors.append(f"{path}: line {line}: unmatched {c}")
            else:
                stack.pop()
        i += 1
    if state in {"block_comment", "string", "byte_string", "char", "byte_char", "raw_string"}:
        errors.append(f"{path}: unterminated lexical state {state}")
    for opening, opening_line in stack:
        errors.append(f"{path}: line {opening_line}: unclosed {opening}")
    return errors


def call_argument_count(text: str, opening_parenthesis: int) -> int | None:
    """Count top-level call arguments while ignoring Rust comments and literals."""
    stack = ["("]
    pairs = {")": "(", "]": "[", "}": "{"}
    state = "code"
    block_depth = 0
    raw_hashes = 0
    arguments = 0
    segment_has_code = False
    i = opening_parenthesis + 1
    while i < len(text):
        c = text[i]
        n = text[i + 1] if i + 1 < len(text) else ""
        if state == "line_comment":
            if c == "\n":
                state = "code"
            i += 1
            continue
        if state == "block_comment":
            if c == "/" and n == "*":
                block_depth += 1
                i += 2
            elif c == "*" and n == "/":
                block_depth -= 1
                i += 2
                if block_depth == 0:
                    state = "code"
            else:
                i += 1
            continue
        if state in {"string", "byte_string", "char", "byte_char"}:
            if c == "\\":
                i += 2
            elif (state in {"string", "byte_string"} and c == '"') or (
                state in {"char", "byte_char"} and c == "'"
            ):
                state = "code"
                i += 1
            else:
                i += 1
            continue
        if state == "raw_string":
            if c == '"' and text.startswith("#" * raw_hashes, i + 1):
                i += 1 + raw_hashes
                state = "code"
            else:
                i += 1
            continue

        if c == "/" and n == "/":
            state = "line_comment"
            i += 2
            continue
        if c == "/" and n == "*":
            state = "block_comment"
            block_depth = 1
            i += 2
            continue
        start = i + 1 if c == "b" and n == "r" else i
        if text[start:start + 1] == "r":
            j = start + 1
            while j < len(text) and text[j] == "#":
                j += 1
            if j < len(text) and text[j] == '"':
                raw_hashes = j - (start + 1)
                state = "raw_string"
                segment_has_code = True
                i = j + 1
                continue
        if c == "b" and n == '"':
            state = "byte_string"
            segment_has_code = True
            i += 2
            continue
        if c == "b" and n == "'":
            state = "byte_char"
            segment_has_code = True
            i += 2
            continue
        if c == '"':
            state = "string"
            segment_has_code = True
            i += 1
            continue
        if c == "'":
            state = "char"
            segment_has_code = True
            i += 1
            continue
        if c in "([{":
            stack.append(c)
            segment_has_code = True
        elif c in ")]}":
            if not stack or stack[-1] != pairs[c]:
                return None
            stack.pop()
            if not stack:
                if segment_has_code:
                    arguments += 1
                return arguments
        elif c == "," and len(stack) == 1:
            if not segment_has_code:
                return None
            arguments += 1
            segment_has_code = False
        elif not c.isspace():
            segment_has_code = True
        i += 1
    return None


def validate_workspace_inheritance(
    root_manifest: dict[str, object],
    crate_manifest: dict[str, object],
    manifest: Path,
    errors: list[str],
) -> None:
    workspace = root_manifest.get("workspace", {})
    if not isinstance(workspace, dict):
        fail(errors, "root workspace table is invalid")
        return
    workspace_package = workspace.get("package", {})
    workspace_dependencies = workspace.get("dependencies", {})
    package = crate_manifest.get("package", {})
    if isinstance(package, dict):
        for key, value in package.items():
            if isinstance(value, dict) and value.get("workspace") is True:
                if not isinstance(workspace_package, dict) or key not in workspace_package:
                    fail(errors, f"{manifest}: package.{key} inherits a missing workspace value")
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        dependencies = crate_manifest.get(section, {})
        if not isinstance(dependencies, dict):
            continue
        for name, value in dependencies.items():
            if isinstance(value, dict) and value.get("workspace") is True:
                if not isinstance(workspace_dependencies, dict) or name not in workspace_dependencies:
                    fail(errors, f"{manifest}: {section}.{name} inherits a missing workspace dependency")
    lints = crate_manifest.get("lints")
    if isinstance(lints, dict) and lints.get("workspace") is True and "lints" not in workspace:
        fail(errors, f"{manifest}: lints inherit from a missing workspace.lints table")


def validate(root: Path, require_package: bool) -> list[str]:
    errors: list[str] = []
    if not root.is_dir():
        return [f"repository root does not exist: {root}"]

    try:
        workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001 - validation boundary
        return [f"Cargo.toml cannot be parsed: {exc}"]
    members = set(workspace.get("workspace", {}).get("members", []))
    expected_members = set(REQUIRED_CRATES.values())
    if members != expected_members:
        fail(errors, f"workspace members mismatch: {sorted(members)}")
    workspace_table = workspace.get("workspace", {})
    if workspace_table.get("resolver") != "3":
        fail(errors, "Rust 2024 virtual workspace must use dependency resolver 3")
    workspace_package = workspace_table.get("package", {})
    if workspace_package.get("edition") != "2024":
        fail(errors, "workspace edition must remain 2024")
    rust_version = workspace_package.get("rust-version")
    if not isinstance(rust_version, str) or not rust_version:
        fail(errors, "workspace rust-version is missing")
    try:
        toolchain = tomllib.loads((root / "rust-toolchain.toml").read_text(encoding="utf-8"))
        channel = toolchain.get("toolchain", {}).get("channel")
        if not isinstance(channel, str) or not channel.startswith(f"{rust_version}."):
            fail(errors, f"rust-toolchain channel {channel!r} does not match rust-version {rust_version!r}")
    except Exception as exc:  # noqa: BLE001
        fail(errors, f"rust-toolchain.toml cannot be parsed: {exc}")

    for crate_name, relative in REQUIRED_CRATES.items():
        crate = root / relative
        manifest = crate / "Cargo.toml"
        lib = crate / "src/lib.rs"
        if not manifest.is_file() or not lib.is_file():
            fail(errors, f"missing crate manifest or lib: {crate_name}")
            continue
        try:
            parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
        except Exception as exc:  # noqa: BLE001
            fail(errors, f"{manifest}: TOML parse failed: {exc}")
            continue
        if parsed.get("package", {}).get("name") != crate_name:
            fail(errors, f"crate name mismatch in {manifest}")
        validate_workspace_inheritance(workspace, parsed, manifest, errors)

    rust_files = sorted(root.glob("crates/**/*.rs"))
    production_rust_files = [path for path in rust_files if "/src/" in path.as_posix()]
    if len(production_rust_files) < 30:
        fail(errors, f"unexpectedly small Rust source surface: {len(production_rust_files)} files")
    for path in rust_files:
        errors.extend(scan_rust(path))
        text = path.read_text(encoding="utf-8")
        if "/src/" in path.as_posix() and FORBIDDEN_PRODUCTION.search(text):
            for match in FORBIDDEN_PRODUCTION.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                fail(errors, f"forbidden production construct: {path.relative_to(root)}:{line}: {match.group(0)}")
        for module in re.findall(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;", text):
            flat = path.parent / f"{module}.rs"
            nested = path.parent / module / "mod.rs"
            if not flat.is_file() and not nested.is_file():
                fail(errors, f"missing module {module} declared by {path.relative_to(root)}")
        for match in re.finditer(r"\bProviderFailure::new\s*\(", text):
            opening = text.find("(", match.start())
            argument_count = call_argument_count(text, opening)
            if argument_count != 2:
                line = text.count("\n", 0, match.start()) + 1
                fail(
                    errors,
                    f"ProviderFailure::new argument mismatch: "
                    f"{path.relative_to(root)}:{line}: {argument_count}",
                )

    # Public facade boundary: implementation seams must not leak from effect crates.
    runtime_lib = (root / "crates/rust-provider-kit-runtime/src/lib.rs").read_text(encoding="utf-8")
    runtime_exports = {
        line.strip()
        for line in runtime_lib.splitlines()
        if line.strip().startswith("pub use ")
    }
    if runtime_exports != EXPECTED_RUNTIME_EXPORTS:
        fail(errors, f"runtime public exports mismatch: {sorted(runtime_exports)}")
    platform_lib = (root / "crates/rust-provider-kit-platform/src/lib.rs").read_text(encoding="utf-8")
    platform_exports = {
        line.strip()
        for line in platform_lib.splitlines()
        if line.strip().startswith("pub use ")
    }
    if platform_exports != EXPECTED_PLATFORM_EXPORTS:
        fail(errors, f"platform public exports mismatch: {sorted(platform_exports)}")
    runtime_source = (root / "crates/rust-provider-kit-runtime/src/runtime.rs").read_text(encoding="utf-8")
    if re.search(r"(?m)^\s*pub\s+fn\s+with_components\b", runtime_source):
        fail(errors, "runtime component injection leaked into the public API")
    loopback_source = (root / "crates/rust-provider-kit-platform/src/loopback.rs").read_text(encoding="utf-8")
    if re.search(r"(?m)^\s*pub\s+async\s+fn\s+prepare_with_opener\b", loopback_source):
        fail(errors, "browser test seam leaked into the public API")
    for relative in (
        "crates/rust-provider-kit-runtime/src/tests.rs",
        "crates/rust-provider-kit-runtime/tests/public_api.rs",
        "crates/rust-provider-kit-platform/src/tests.rs",
        "crates/rust-provider-kit-platform/tests/public_api.rs",
    ):
        if not (root / relative).is_file():
            fail(errors, f"contract test is missing: {relative}")

    # Swift package-private implementation maps to Rust crate visibility. Plain
    # `pub` is reserved for the actual facade and platform surface.
    allowed_plain_public = {
        "crates/rust-provider-kit-runtime/src/runtime.rs": {"ProviderRuntime"},
        "crates/rust-provider-kit-runtime/src/openrouter_oauth.rs": {
            "OpenRouterOAuthRegistrationRequest"
        },
        "crates/rust-provider-kit-runtime/src/in_memory_credential_store.rs": {
            "InMemoryProviderCredentialStore"
        },
        "crates/rust-provider-kit-platform/src/loopback.rs": {
            "PreparedLoopbackAuthorization",
            "LoopbackAuthorizationSession",
        },
        "crates/rust-provider-kit-platform/src/pkce.rs": {"ProviderPkceGenerator"},
    }
    plain_public = re.compile(
        r"(?m)^\s*pub\s+(?:struct|enum|trait|type)\s+([A-Za-z_][A-Za-z0-9_]*)"
    )
    for crate_root in (
        root / "crates/rust-provider-kit-runtime/src",
        root / "crates/rust-provider-kit-platform/src",
    ):
        for path in crate_root.rglob("*.rs"):
            relative = path.relative_to(root).as_posix()
            allowed = allowed_plain_public.get(relative, set())
            for item in plain_public.findall(path.read_text(encoding="utf-8")):
                if item not in allowed:
                    fail(errors, f"internal implementation type has plain pub visibility: {relative}::{item}")
    if "with_chunk_capacity" in (root / "crates/rust-provider-kit-runtime/src/http_transport.rs").read_text(encoding="utf-8"):
        fail(errors, "redundant HTTP chunk-capacity constructor remains")

    # Dependency direction: Core must remain effect-free and platform/runtime independent.
    core_text = "\n".join(path.read_text(encoding="utf-8") for path in (root / "crates/rust-provider-kit-core/src").glob("*.rs"))
    if "rust_provider_kit_runtime" in core_text or "rust_provider_kit_platform" in core_text or "reqwest::" in core_text:
        fail(errors, "core imports an effect crate")
    runtime_text = "\n".join(path.read_text(encoding="utf-8") for path in (root / "crates/rust-provider-kit-runtime/src").rglob("*.rs"))
    platform_text = "\n".join(path.read_text(encoding="utf-8") for path in (root / "crates/rust-provider-kit-platform/src").rglob("*.rs"))
    if "rust_provider_kit_platform" in runtime_text or "rust_provider_kit_runtime" in platform_text:
        fail(errors, "runtime and platform crates are directly coupled")

    # Production code must import private implementation from its owning module,
    # never through a crate-root prelude/re-export assumption.
    for crate_root in (
        root / "crates/rust-provider-kit-runtime/src",
        root / "crates/rust-provider-kit-platform/src",
    ):
        for path in crate_root.rglob("*.rs"):
            if path.name in {"lib.rs", "tests.rs"}:
                continue
            text = path.read_text(encoding="utf-8")
            for item in re.findall(r"\bcrate::([A-Z][A-Za-z0-9_]*)", text):
                fail(
                    errors,
                    f"implementation imports {item} through crate root instead of its owner module: "
                    f"{path.relative_to(root)}",
                )

    identifiers = (root / "crates/rust-provider-kit-core/src/identifiers.rs").read_text(encoding="utf-8")
    registry = (root / "crates/rust-provider-kit-runtime/src/registry.rs").read_text(encoding="utf-8")
    required_ids = ["codex", "openai", "anthropic", "gemini", "openrouter", "deepseek", "qwen", "kimi", "zai", "minimax"]
    for value in required_ids:
        if f'"{value}"' not in identifiers or value not in registry.lower():
            fail(errors, f"built-in provider is not represented in ID and registry: {value}")

    legacy_identifier = re.compile(
        r"\b(?:" + "So" + "A|So" + "a|so" + "a)\b|" + "so" + "a_"
    )
    for path in root.rglob("*"):
        if not path.is_file() or path == root / "docs/SWIFT_CONTRACTS.json":
            continue
        relative = path.relative_to(root)
        if relative.parts and relative.parts[0] in IGNORED_ROOT_PARTS:
            continue
        if path.suffix not in {".rs", ".py", ".md", ".toml"}:
            continue
        text = path.read_text(encoding="utf-8")
        if relative.as_posix() == "FEATURE_MATRIX.md":
            for line in text.splitlines():
                columns = [column.strip() for column in line.strip().strip("|").split("|")]
                if len(columns) == 8 and legacy_identifier.search("|".join(columns[:2] + columns[3:])):
                    fail(errors, "FEATURE_MATRIX contains a legacy identifier outside Swift evidence")
            continue
        if legacy_identifier.search(text):
            fail(errors, f"legacy provider identifier remains outside raw Swift provenance: {relative}")

    forbidden_absolute_prefixes = ("/" + "Applications/", "/" + "usr/local/", "/" + "opt/homebrew/")
    for path in root.rglob("*.rs"):
        text = path.read_text(encoding="utf-8")
        for prefix in forbidden_absolute_prefixes:
            if prefix in text:
                fail(errors, f"hard-coded executable path remains: {path.relative_to(root)}")

    # Repository hygiene.
    for path in root.rglob("*"):
        relative = path.relative_to(root)
        if relative.parts and relative.parts[0] in IGNORED_ROOT_PARTS:
            continue
        if any(part in CACHE_PARTS for part in relative.parts):
            continue
        if path.is_symlink():
            fail(errors, f"symlink present: {relative}")
        if path.is_file() and path.name.lower().endswith(ARCHIVE_SUFFIXES):
            fail(errors, f"nested archive present: {relative}")
        posix = PurePosixPath(relative.as_posix())
        if posix.is_absolute() or ".." in posix.parts:
            fail(errors, f"unsafe relative path: {relative}")
    for relative in OBSOLETE_PATHS:
        if (root / relative).exists():
            fail(errors, f"obsolete migration/duplicate artifact is present: {relative}")
    for relative in (
        "README.md",
        "COMPLETION_REPORT.md",
        "VALIDATION_REPORT.md",
        "VERIFY_LOCAL.md",
        "docs/INTERFACE_CONTRACT.md",
    ):
        path = root / relative
        if path.is_file() and "/mnt/data/" in path.read_text(encoding="utf-8"):
            fail(errors, f"packaged document contains a workspace-local absolute path: {relative}")
    for relative in ("README.md", "docs/INTERFACE_CONTRACT.md"):
        path = root / relative
        if path.is_file():
            text = path.read_text(encoding="utf-8")
            if "with_components" in text or "PARITY_REPORT.md" in text:
                fail(errors, f"public documentation references an internal or removed surface: {relative}")

    matrix = root / "FEATURE_MATRIX.md"
    if matrix.is_file():
        matrix_text = matrix.read_text(encoding="utf-8")
        rows = [
            line
            for line in matrix_text.splitlines()
            if re.match(r"^\|\s*SWIFT-\d{3}\s*\|", line)
        ]
        expected_ids = [f"SWIFT-{index:03d}" for index in range(1, 93)]
        actual_ids = [row.split("|")[1].strip() for row in rows]
        if actual_ids != expected_ids:
            fail(errors, f"FEATURE_MATRIX IDs mismatch: {actual_ids}")
        for row in rows:
            columns = [column.strip() for column in row.strip().strip("|").split("|")]
            if len(columns) != 8 or columns[5] != "COMPLETE":
                fail(errors, f"FEATURE_MATRIX row is not source-complete: {row}")
            if not columns[6]:
                fail(errors, f"FEATURE_MATRIX row has no validation state: {row}")
        for relative in sorted(set(re.findall(r"`((?:crates|scripts)/[^`]+)`", matrix_text))):
            if not (root / relative).exists():
                fail(errors, f"FEATURE_MATRIX references a missing repository path: {relative}")
        snapshot = root / "docs/SWIFT_CONTRACTS.json"
        if not snapshot.is_file():
            fail(errors, "Swift contract snapshot is missing")
        else:
            try:
                import json
                payload = json.loads(snapshot.read_text(encoding="utf-8"))
                contracts = payload.get("contracts", [])
                if payload.get("contract_count") != 92 or len(contracts) != 92:
                    fail(errors, "Swift contract snapshot count is not 92")
            except Exception as exc:  # noqa: BLE001
                fail(errors, f"Swift contract snapshot is invalid: {exc}")

    if require_package:
        for relative in REQUIRED_REPORTS:
            if not (root / relative).is_file():
                fail(errors, f"required packaged artifact is missing: {relative}")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", nargs="?", default=Path(__file__).resolve().parents[1])
    parser.add_argument("--require-package", action="store_true")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    errors = validate(root, args.require_package)
    if errors:
        for error in errors:
            print(f"FAIL: {error}")
        return 1
    print(f"PASS: source structure and static contracts ({root})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
