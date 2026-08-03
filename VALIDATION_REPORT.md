# VALIDATION_REPORT

## 대상과 환경

- Repository: workspace root (`.`)
- Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `aarch64-apple-darwin`
- Dependency resolution: root `Cargo.lock`, all Rust commands use `--locked`
- 검증 lens: core + systems (state, interface, concurrency, partial failure)
- 상태: `LOCAL_VERIFIED / EXTERNAL_UNVERIFIED`

## 명령 결과

| 명령 또는 검사 | 결과 | 보장 범위 |
|---|---:|---|
| `cargo fmt --all -- --check` | PASS | canonical formatting |
| `cargo check --workspace --all-targets --all-features --locked` | PASS | types, traits, all targets/features |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | PASS | workspace lint warnings 0 |
| `cargo test --workspace --all-targets --all-features --locked` | PASS | 85 passed, 0 failed |
| `cargo test --workspace --no-default-features --locked` | PASS | default-feature-independent build/test mode |
| `python3 scripts/validate_source.py .` | PASS | workspace, dependency direction, facade, forbidden production constructs, hygiene |
| `python3 scripts/validate_swift_matrix.py .` | PASS | Swift contracts 92, rows 92, Rust tests 85 |
| `python3 scripts/audit_complexity.py .` | PASS | 44 files, 16,743 lines, documented coverage, front-shift 없음 |
| `python3 -m py_compile scripts/*.py` | PASS | audit/matrix/package scripts syntax |
| source archive→safe clean extraction→재검증 | PASS | archive metadata, safe paths, no symlink/cache/secret/nested archive, extracted gates |
| actual 10-provider credential calls | `[UNVERIFIED]` | credential/service 권한을 사용하지 않음 |
| actual system-browser OAuth | `[UNVERIFIED]` | 사용자 browser를 열지 않음 |

## 테스트 분포

| Suite | Passed |
|---|---:|
| Core contracts | 27 |
| Platform unit | 8 |
| Platform public API | 2 |
| Runtime unit/characterization | 46 |
| Runtime public API | 2 |
| 합계 | 85 |

## 변경 behavior→runtime surface→proof

| 변경 behavior | Runtime surface | Happy/edge/failure proof | 결과 |
|---|---|---|---:|
| request clone이 payload 크기에 비례하지 않음 | `ProviderTurnRequest`, execution reducer/session | serde/policy round-trip, reducer/stream 전체 suite | PASS |
| reconciliation과 registration 상호 배타 | `ProviderRuntime`, account supervisor | reconciliation 중 direct register reject; OAuth 중 reconciliation reject | PASS |
| revoke가 in-flight inspect를 추월하지 않음 | `inspect`, `revoke`, account cache/vault | blocked HTTP inspection 동안 credential 유지, 완료 후 remove/cache empty | PASS |
| account-scoped revoke | execution supervisor account index | 두 account 실행 중 한 account만 cancel/remove | PASS |
| SSE reconnect-only 상태 제거 | SSE decoder | multiline/CRLF/BOM/bounds + 32KiB ignored id/retry | PASS |
| Codex file/process 경계 | secure reader/version resolver | symlink/oversize reject, managed metadata, single-flight, executable qualification | PASS |
| malformed usage overflow | wire normalization | upstream overflow→`MalformedResponse` | PASS |
| execution terminal ordering | reducer/session/event stream | cleanup 후 terminal, started state, producer loss, mailbox overflow | PASS |

## 시나리오 분류

- Product failure: 현재 local suite에서 없음.
- Environment/external gap: live Provider credentials/service, 실제 system browser OAuth.
- Coverage gap: `FEATURE_MATRIX.md` 92행 중 34행은 compile+source trace만 있고 직접 scenario test가 없다.

## 보장하지 않는 범위

- 실제 Provider wire와 policy가 fixture 이후 변경되지 않았는지
- real durable vault의 locking/transaction semantics
- 실제 browser application과 loopback socket의 사용자 환경 동작
- production load에서 JSON conversion allocation과 queue sizing
- 실제 공개 release revision

## Confidence

- Local source/build/lint/test: `높음`
- Mock 기반 state/concurrency/failure behavior: `높음`
- 전체 92 Swift parity의 runtime proof: `중간` (34 source-trace gaps)
- Live release readiness: `낮음` (external flow 미검증)
