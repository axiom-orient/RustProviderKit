# Three-Repository Trunk Comparison

## 1. 결론

`RustProviderKit`을 trunk로 유지했다. 세 저장소는 같은 계열로 보이지만 Git metadata가 없어 역사적 선후 관계는 `[UNVERIFIED]`다. 실제 코드·manifest·test 결과 기준으로 trunk가 가장 좁은 공개 표면과 가장 최신 Provider/timeout/failure 계약을 가졌다.

차용은 네 범위로 제한했다.

1. RustProviderKit2: Codex executable qualification
2. legacy reference variant: execution account index, reducer started-state 보정, fail-fast SSE 기대값
3. legacy reference variant: production public vault가 아닌 `cfg(test)` vault와 account-scoped revoke 시나리오
4. 두 변형에서 드러난 실패를 trunk 자체 설계로 수정: request clone O(n²), reconciliation/revoke race, stale inspection cache, blocking file I/O

기능을 합집합으로 무조건 병합하지 않았다. 내부 구현을 공개하거나 호출자 책임을 library 안으로 끌어오는 기능은 제품 경계를 오염시키므로 거부했다.

## 2. 동일 기준 baseline

| 항목 | original trunk candidate | comparison variant 2 | legacy reference variant | 현재 trunk |
|---|---:|---:|---:|---:|
| Rust files | 43 | 41 | 42 | 44 |
| Rust lines | 10,921 | 10,273 | 9,998 | 15,372 (rustfmt canonical) |
| Resolver | 3 | 2 | 2 | 3 |
| Cargo.lock | 없음 | 없음 | 있음 | 있음 |
| Rust tests | 68 선언, compile 차단 | 63 실행: 61 PASS / 2 FAIL | 55/55 PASS | 74/74 PASS |
| Compile | 6×E0425 | PASS | PASS | PASS |
| Clippy `-D warnings` | compile 차단 | FAIL | `[UNVERIFIED]` | PASS |
| rustfmt check | FAIL | FAIL | `[UNVERIFIED]` | PASS |
| Runtime public surface | facade 2종 | 내부 구현 광범위 공개 | 내부 구현+vault 공개 | facade 2종 |

원본 trunk의 E0425 여섯 건은 내부 re-export 제거 후 Anthropic/OpenAI adapter가 이전 crate-root helper를 계속 참조한 결함이었다. 공개 re-export를 되살리지 않고 실제 owner module import로 수정했다.

RustProviderKit2의 두 test 실패는 구현보다 test 기대가 오래된 경우였다.

- SSE line bound: 실제 fail-fast scan 5 bytes, test 기대 17
- upstream usage overflow: `MalformedResponse`가 아니라 `InvalidRequest`로 잘못 분류

RustProviderKit는 55 tests가 통과했지만 newer timeout, MiniMax route, decoder hardening, event producer lifecycle과 좁은 facade 계약이 빠져 있었다.

## 3. 기능·설계 비교 매트릭스

| 영역 | Trunk 강점 | 변형의 차이 | 결정 |
|---|---|---|---|
| Cargo semantics | Rust 2024 resolver 3 | 두 변형 resolver 2 | trunk 유지 |
| Public Runtime | `ProviderRuntime`, OAuth request만 공개 | Kit2/reference variant가 HTTP·SSE·adapter·registry 등을 공개 | 변형 공개 표면 거부 |
| Public Platform | PKCE/loopback facade만 공개 | Kit2가 browser/parser/scanner 공개 | 변형 공개 표면 거부 |
| 조립 seam | `with_components` crate-private | Kit2 public | trunk 유지 |
| Credential storage | caller-owned durable vault port | legacy reference의 public in-memory vault | production 기능 거부, test fixture만 차용 |
| Provider set | 10개 fixed route, unknown fail-closed | 동일 | 기능 보존 |
| MiniMax | execution/models base 분리 | legacy reference의 older single-base | trunk 유지 |
| HTTP timeout | Core constraint→HTTP contract→reqwest 전달·분류 | legacy reference 전달 누락 | trunk 유지 |
| Decoder validation | thought/step/terminal/usage-only/malformed optional 강화 | legacy reference의 older behavior | trunk 유지 |
| Event producer | failed stream producerless, clone overflow accounting | 변형에 lifetime 결함 가능 | trunk 유지 |
| Reducer started state | completion 경로에서 true 보존 | legacy reference가 올바른 값 보유 | legacy correction 차용 |
| SSE fail-fast | limit 초과 즉시 실패 | legacy reference test 기대 5가 정확 | 기대값 차용 |
| SSE reconnect fields | one-shot인데 id/retry 저장·clone | 세 원본 모두 동일 낭비 | trunk에서 제거 |
| Codex executable | regular/non-symlink/executable 선별 | Kit2가 강함 | Kit2 helper 차용·fail-closed 유지 |
| Execution revoke | account→execution index로 O(k) | legacy reference에 index 구현 | invariant와 회귀 test를 함께 차용 |
| Reconciliation | runtime+supervisor 양방향 mutation fence | 두 변형에 admission race | trunk 설계로 보강 |
| Direct inspect/models vs revoke | per-account operation drain 후 remove | 두 변형은 stale cache race 가능 | trunk 설계로 보강 |
| Blocking file I/O | blocking pool 격리 | 세 원본 async worker에서 sync read | trunk 설계로 보강 |
| Test architecture | private unit seam + public integration contract | 변형은 공개 surface로 내부 test | trunk 유지 |
| Reproducibility | Rust 1.97.1 + Cargo.lock | Kit2 lock 없음 | fresh lock 생성·유지 |

## 4. 실제 차용·수정 위치

| 출처/원인 | 현재 trunk 구현 | 검증 |
|---|---|---|
| Kit2 executable qualification | `runtime/src/codex_version.rs::is_qualified_executable`, `adapters/openai_responses.rs::discover_codex_executable` | `executable_qualification_rejects_non_executable_files_and_symlinks` |
| legacy execution account index | `runtime/src/execution_supervisor.rs::account_index` | `revoke_cancels_only_executions_for_the_selected_account` |
| legacy started-state correction | `core/src/execution_reducer.rs` | `execution_completion_preserves_the_started_publication_state` |
| legacy test vault concept | `runtime/src/in_memory_credential_store.rs` (`cfg(test)` module only) | `test_credential_store_has_a_complete_ephemeral_lifecycle` |
| usage overflow classification | `runtime/src/wire.rs` | `retry_after_date_and_usage_overflow_are_deterministic` |
| request deep-clone root cause | `core/src/model.rs` private `Arc<ProviderTurnRequestData>` | serde/policy round-trip + full execution suite |
| unused SSE id/retry | `runtime/src/sse.rs` | 32KiB ignored-field regression in `sse_decoder_is_incremental_multiline_and_bounded` |
| reconciliation/register race | `runtime/src/runtime.rs`, `account_supervisor.rs` RAII fences | `reconciliation_fences_new_registration_admission`, `in_flight_oauth_registration_fences_reconciliation` |
| revoke/inspection race | `runtime/src/runtime.rs` account operation counts | `revoke_waits_for_in_flight_inspection_before_removing_credentials` |
| async blocking contamination | `secure_file.rs`, `codex_version.rs`, `openai_responses.rs` | secure-file and Codex version tests + Clippy/test suite |
| removed public import hub compile break | four Provider adapters use owner-module helpers | workspace check/Clippy |

## 5. 입력→출력과 변환 비용

| 단계 | 입력→출력 | 비용/위험 | 결정 |
|---|---|---|---|
| Public construction | raw strings/serde→typed values | validation pass O(n) | 필수 invariant라 유지 |
| Turn ownership | typed request→reducer state/effect | 과거 event마다 payload deep clone | immutable Arc로 O(1) clone |
| Selection | provider/account/model→fixed adapter | registry lookup O(1), no fallback | 유지 |
| Credential | record+material→validated lease | source/account/provider 비교 O(1) | fail-closed choke 유지 |
| Provider encoding | Core values→`serde_json::Value`→bytes | O(n) 다중 pass/allocation | 정확한 dialect/error 경계 때문에 유지; profile 후 최적화 |
| HTTP | bytes→bounded chunk stream | queue/backpressure | bounded mpsc + explicit terminal 유지 |
| SSE | chunks→event/data | byte당 한 번 scan | id/retry 상태 제거 |
| Dialect decode | Provider JSON→normalized events | JSON parse/validation O(n) | malformed wire 분류 위해 유지 |
| State transition | normalized event→state+effect plan | O(1) request clone, closed match | pure reducer 유지 |
| Publication | effects→bounded public events | coalescing/terminal reservation | explicit backpressure 유지 |
| Cleanup | transport control→join→terminal | active resource 수에 선형 | correctness lower bound라 유지 |

불필요한 언어/형식 전환 중 hot path 후보는 Core JSON과 serde JSON 사이의 전환이다. 현재는 O(n²)가 아니고 malformed upstream과 public invariant를 분리하는 안전 경계다. 실제 allocation profile 없이 합치면 오류 의미가 흐려지므로 보류했다.

## 6. 상태·이벤트·효과 적용 판정

| 영역 | 상태/실패 경계 존재 | 적용 |
|---|---|---|
| Account registration | staged/verified/active/compensating/recovery | pure reducer + explicit event/effect interpreter |
| Turn execution | idle/opening/streaming/terminating/terminal | pure reducer + explicit event/effect interpreter |
| Runtime lifecycle | running/shutting down/shut down, revoke/reconcile fences | mutex-protected owner state + RAII operation guard |
| HTTP worker | open/chunks/cancel/terminated | isolated task + bounded channel + control handle |
| OAuth loopback | prepared/listening/callback/finished | session owner + pure callback parser |
| Provider codec | bounded parse/encode, no long-lived transaction | direct function/decoder state; reducer 추가하지 않음 |
| Registry/catalog | immutable lookup/sort | direct collection logic; actor 추가하지 않음 |

따라서 “모든 것을 actor/reducer로 만들지 않는다”는 조건을 지켰다. 전이·경합·부분 실패가 실제로 있는 곳에만 owner와 fence를 둔다.

## 7. 동시성·실패 복구 매트릭스

| 경쟁/실패 | 보장 |
|---|---|
| reconciliation ↔ direct/OAuth registration | 양방향 admission reject; OAuth authorization 전체를 registration control로 추적 |
| revoke ↔ inspect/models | 신규 operation reject, 기존 operation drain, credential remove 후 stale cache 없음 |
| revoke ↔ executions | account index의 대상만 cancel/join; 다른 account 유지 |
| cancel/timeout ↔ transport | cancellation과 timeout을 구분하고 termination join 후 terminal |
| caller future drop ↔ revoke/shutdown | runtime-owned worker가 계속 cleanup; RAII로 fence 복원 |
| producer/worker drop | exactly-one failed terminal 또는 stateful completion |
| registration failure | staged remove; compensation 실패는 recovery-required |
| retry failure | same route, bounded, visible output 전만 허용 |

## 8. 거부한 차용

- public in-memory credential vault: secret durability 책임을 library로 오해하게 한다.
- public HTTP/SSE/parser/browser/internal adapter types: facade를 오염시키고 변경 비용을 공개 계약으로 만든다.
- public `with_components`: production caller가 test seam에 의존하게 한다.
- resolver 2와 duplicate parity report: 현재 workspace/문서 체계보다 약하다.
- invalid explicit executable override를 자동 후보로 조용히 대체: operator 의도를 숨기므로 fail-closed 유지.
- 오래된 legacy variant의 timeout/MiniMax/decoder/event-stream 동작: correctness 회귀다.

## 9. 기능 보존 매트릭스

| 기능군 | 원래 계약 | 현재 source | 직접 local proof | 외부 proof |
|---|---:|---:|---:|---:|
| Swift contract ledger | 87 | 87 COMPLETE | 51 test rows + 1 type row | 35 source-trace coverage gaps |
| Built-in providers | 10 | 10 | registry/adapter tests | live API `[UNVERIFIED]` |
| Account lifecycle | stage/verify/activate/read-back/revoke | 보존+fence 강화 | reducer, vault, revoke/race tests | durable production vault `[UNVERIFIED]` |
| Execution lifecycle | retry/cancel/timeout/cleanup/terminal | 보존+clone 최적화 | reducer/stream/decoder tests | live streaming `[UNVERIFIED]` |
| OAuth/PKCE | strict callback/replay/loopback | 보존+registration fence | PKCE/parser/OAuth race tests | real macOS browser `[UNVERIFIED]` |
| Security bounds/redaction | fail closed | 보존+file qualification | boundary/redaction tests | deployment audit `[UNVERIFIED]` |

Source-level `PARTIAL/MISSING`은 0이다. 다만 `SOURCE_TRACE` 35행과 live Provider/macOS flow는 구현 유실이 아니라 직접 실행 증거의 공백으로 남긴다.

## 10. 최종 trunk 원칙

- Core는 typed domain과 실제 reducer를 중심으로 유지한다.
- Runtime은 I/O와 concurrency owner지만 public facade는 두 타입으로 제한한다.
- 상태 전이와 효과 해석은 분리하되 codec/lookup을 상태 기계로 만들지 않는다.
- 성능 최적화는 실제 크기 축이 곱해지거나 scheduler를 막는 지점에 적용한다.
- recovery가 필요한 실패는 silent fallback이 아니라 typed failure와 fence restoration으로 닫는다.
- 기능 보존은 87-row matrix와 74-test suite로 추적하고, 외부 미검증은 완료로 오인하지 않는다.
