#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

[[ ! -d .github/workflows ]] || fail '.github/workflows is prohibited'

for path in \
  COMPLETION_REPORT.md \
  COMPLEXITY_AUDIT.md \
  FEATURE_MATRIX.md \
  RELEASE.md \
  TRUNK_COMPARISON_REPORT.md \
  VALIDATION_REPORT.md \
  VERIFY_LOCAL.md \
  docs/GAJAE_PROVIDER_PARITY.md \
  docs/PARITY_MATRIX.md \
  docs/SWIFT_CONTRACTS.json \
  scripts/audit_complexity.py \
  scripts/snapshot_swift_contracts.py \
  scripts/validate_source.py \
  scripts/validate_swift_matrix.py \
  scripts/cleanup_inventory.sh; do
  [[ ! -e "$path" ]] || fail "legacy repository path remains: $path"
done

extra_script=$(find scripts -maxdepth 1 -type f \
  ! -name 'verify.sh' ! -name 'clean.sh' -print -quit)
[[ -z "$extra_script" ]] || fail "unexpected script remains: $extra_script"

runtime_source=crates/rust-provider-kit-runtime/src
codex_source="$runtime_source/codex_version.rs"
responses_source="$runtime_source/adapters/openai_responses.rs"
if rg -n 'std::process::Command|tokio::process|split_paths' "$runtime_source"; then
  fail 'provider runtime reaches a host process or PATH search'
fi
if rg -n 'std::path::Path|PathBuf|std::env::var_os|spawn_blocking|version\.json|CodexClientVersion|is_qualified_executable|discover_codex_executable|ARA_PROVIDER_KIT_CODEX_EXECUTABLE' "$codex_source" "$responses_source"; then
  fail 'Codex version resolution still probes host state'
fi
if ! rg -q 'pub struct ProviderRuntimeOptions' "$runtime_source/runtime.rs"; then
  fail 'ProviderRuntimeOptions is not public'
fi
if ! rg -q 'pub fn with_options' "$runtime_source/runtime.rs"; then
  fail 'ProviderRuntime::with_options is missing'
fi
if ! rg -q 'codex_client_version' "$runtime_source/registry.rs"; then
  fail 'registry does not receive the Codex version option'
fi
if ! rg -q 'fn failure_context' "$runtime_source/adapter.rs" "$responses_source"; then
  fail 'Codex failure context contract is missing'
fi
if ! rg -q 'failure_context\(status\)' "$runtime_source/execution_session.rs"; then
  fail 'HTTP failures do not consume adapter context'
fi
if ! rg -q 'precedence_is_explicit_config_then_environment_then_built_in' "$codex_source"; then
  fail 'Codex version precedence regression test is missing'
fi
if ! rg -q 'runtime_rejects_an_invalid_configured_codex_version_during_construction' "$runtime_source/tests.rs"; then
  fail 'invalid configured Codex version construction test is missing'
fi
if ! rg -q 'ProviderRuntimeOptions' crates/rust-provider-kit-runtime/tests/public_api.rs; then
  fail 'public API regression test does not cover ProviderRuntimeOptions'
fi
codex_cli_option=$(printf '%s%s' '--codex-' 'client-version')
if rg -n -F -- "$codex_cli_option" crates/rust-provider-kit-runtime; then
  fail 'provider-kit source names an upper-layer CLI option'
fi
if ! rg -q 'ProviderRuntimeOptions|ARA_PROVIDER_KIT_CODEX_CLIENT_VERSION' README.md docs/INTERFACE_CONTRACT.md; then
  fail 'runtime option contract is missing from documentation'
fi

git diff --check
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets --all-features

printf '%s\n' 'PASS: local Rust verification'
