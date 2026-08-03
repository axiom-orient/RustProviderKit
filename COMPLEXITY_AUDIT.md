# Complexity and Boundary Audit

## 1. 결론

| 항목 | 현재 판정 |
|---|---|
| 전수 범위 | Rust 44 files / 16,743 lines |
| 테스트 | 85 declarations, 전체 PASS |
| 최대 파일 | test: `runtime/src/tests.rs` 1,823 lines; production: `core/src/model.rs` 1,359 lines |
| 최대 함수 | `adapters/gemini.rs::encode_request` 217 lines, bounded dialect translation |
| 중첩 loop 파일 | 7 |
| 현재 unbounded O(n²) | **없음** |
| 제거한 실제 O(n²) | request deep clone × execution events; persistent SSE id clone × events |
| 남은 성능 위험 | 여러 O(n) JSON 변환/검증 pass와 allocation; 측정 전에는 경계 정확성을 위해 유지 |

라인 수는 결함 판정이 아니다. 책임 owner, 입력 크기, 반복 관계, allocation, await/lock 위치, 실패 복구와 공개 계약을 함께 판정했다.

## 2. 분석 기준

`scripts/audit_complexity.py`로 모든 Rust 파일의 함수 크기, loop depth, loop 내부 재스캔, sort, await, lock, front-shift를 계측하고 수동으로 다음을 확인했다.

- 중첩 loop가 같은 collection을 반복 스캔하는지, 서로 겹치지 않는 partition인지
- request/event 수처럼 독립적으로 커질 수 있는 두 축이 곱해지는지
- cloning이 실제 payload deep copy인지 reference count 증가인지
- control path 최적화가 hot path state/invariant를 과도하게 늘리는지
- async executor에서 blocking I/O를 수행하는지

## 3. 제거한 실제 O(n²)와 불필요한 비용

| 기존 비용 | 복잡도 | 조치 | 현재 |
|---|---:|---|---:|
| execution event마다 `ProviderExecutionState` clone이 큰 `ProviderTurnRequest`를 deep-copy | O(R×E) | request private storage를 immutable `Arc`로 변경 | state clone O(1), 전체 O(R+E) |
| persistent SSE `id`를 event마다 clone; adapter는 id/retry를 소비하지 않음 | O(I×E) | one-shot stream에서 미사용 `id/retry` 상태·출력 제거 | wire scan O(n) |
| revoke가 모든 active execution을 scan | O(n) | `account_index: account→execution IDs` 추가 | 대상 account 기준 O(k) |
| async Codex path의 동기 auth/version read와 metadata probe | scheduler stall 가능 | `spawn_blocking` 경계로 이동 | executor 격리 |
| 부적격 executable을 선택한 뒤 spawn 실패 | 불필요한 실패/재작업 | regular·non-symlink·executable qualification | 후보당 O(1) metadata |
| retry body deep copy, front insertion, text/SSE vector+join | 반복 allocation/shift | `Bytes`, append accumulator, direct order 구성 | 선형 |
| 사용하지 않는 public implementation re-export | 변경 전파/호환 비용 | facade만 공개하고 owner module 직접 참조 | 내부 변경 격리 |

R=request payload 크기, E=decoded event 수, I=persistent event-id 길이, n=전체 active execution, k=해지 대상 account의 execution 수다.

## 4. 중첩 loop 7개 판정

| 파일 | 판정 | 근거 |
|---|---|---|
| `crates/rust-provider-kit-runtime/src/adapters/anthropic.rs` | O(total content) | message의 child content는 겹치지 않는 partition이다. |
| `crates/rust-provider-kit-runtime/src/adapters/gemini.rs` | O(total content) | 각 message/content item을 한 번 직렬화한다. |
| `crates/rust-provider-kit-runtime/src/adapters/openai_chat.rs` | O(total content) | message child를 한 번 방문하며 front shift가 없다. |
| `crates/rust-provider-kit-runtime/src/adapters/openai_responses.rs` | O(total content) | input item과 child content가 partition 관계다. |
| `crates/rust-provider-kit-runtime/src/execution_session.rs` | O(wire bytes + events) | chunk→SSE→decoded event가 계층별 partition이다. |
| `crates/rust-provider-kit-runtime/src/registration_session.rs` | O(events) | 한 event의 effect fan-out은 closed reducer transition으로 제한된다. |
| `crates/rust-provider-kit-runtime/src/sse.rs` | O(bytes) | inner loop는 최초 UTF-8 BOM 최대 3 bytes뿐이다. |

## 5. 의도적으로 유지한 비용

| 비용 | 판정 |
|---|---|
| model/account catalog sort O(n log n) | 공개 결과의 결정적 순서와 duplicate rejection을 위해 유지 |
| shutdown O(n) cancel/join | 모든 active work를 종료해야 하므로 lower bound에 가깝다. |
| request tool-name `BTreeSet` O(t log t) | 최대 128개, duplicate/named choice invariant를 한 번에 닫는다. |
| JSON domain↔`serde_json::Value` 여러 O(n) pass | wire malformed와 public invariant를 분리하는 validation choke다. O(n²)는 아니며 profile 없이 제거하지 않는다. |
| Provider wire와 Core 양쪽 validation | 외부 실패 분류와 public domain invariant의 owner가 다르다. |
| bounded callback/query/header scan O(n) | byte/count/time 상한이 있고 security validation에 필요하다. |

향후 실제 profile에서 JSON allocation이 병목으로 확인될 때만 borrowed wire DTO 또는 bounded serializer로 합친다. 지금 합치면 Provider별 오류 분류와 Core validation 경계가 오염된다.

## 6. Rust 파일별 전수 판정

`최대 함수`는 함수명/라인, `D/S/A/L`은 loop depth / loop 안 scan / await / lock 수다.

| 파일 | Lines | 최대 함수 | D/S/A/L | 역할과 판정 |
|---|---:|---|---|---|
| `crates/rust-provider-kit-core/src/account_reducer.rs` | 326 | reduce/210 | 0/0/0/0 | account pure transition의 exhaustive authority. 긴 match를 유지한다. |
| `crates/rust-provider-kit-core/src/accounts.rs` | 864 | validate_request_headers/68 | 1/5/1/0 | account·credential 값과 ports. header 검증은 전체 입력을 선형으로 제한한다. |
| `crates/rust-provider-kit-core/src/error.rs` | 70 | validate_trimmed_text/12 | 0/0/0/0 | Core validation error. 유지. |
| `crates/rust-provider-kit-core/src/event_stream.rs` | 609 | send/48 | 1/0/2/9 | bounded mailbox, terminal reservation, producer lifetime authority. 유지. |
| `crates/rust-provider-kit-core/src/events.rs` | 433 | deserialize/24 | 0/0/0/0 | public event/failure/usage contract. 유지. |
| `crates/rust-provider-kit-core/src/execution_reducer.rs` | 332 | reduce/214 | 0/0/0/0 | execution pure transition의 exhaustive authority. 유지. |
| `crates/rust-provider-kit-core/src/identifiers.rs` | 235 | validate_provider/15 | 0/0/0/0 | typed IDs와 fail-closed validation. 유지. |
| `crates/rust-provider-kit-core/src/instant.rs` | 52 | from_system_time/13 | 0/0/0/0 | bounded timestamp value. 유지. |
| `crates/rust-provider-kit-core/src/json_value.rs` | 316 | validate_at/56 | 1/1/0/0 | depth/node/string/key/encoded-size bounded JSON. 각 node 선형 방문. |
| `crates/rust-provider-kit-core/src/lib.rs` | 29 | - | 0/0/0/0 | Core export wiring만 소유. |
| `crates/rust-provider-kit-core/src/model.rs` | 1,359 | new/81 | 1/1/0/0 | turn/message/tool/model domain. request Arc로 deep clone 제거; public contract 응집 때문에 유지. |
| `crates/rust-provider-kit-core/src/oauth.rs` | 68 | new/22 | 0/0/0/0 | PKCE/authorization 값. 유지. |
| `crates/rust-provider-kit-core/tests/core_contracts.rs` | 815 | execution_publishes_terminal_only_after_cleanup/50 | 0/0/19/0 | public Core regression tests. production 분리 대상 아님. |
| `crates/rust-provider-kit-platform/src/lib.rs` | 15 | - | 0/0/0/0 | Platform facade wiring. |
| `crates/rust-provider-kit-platform/src/loopback.rs` | 381 | authorize/73 | 1/0/13/2 | listener/browser/connection lifecycle owner. parser는 분리됨. |
| `crates/rust-provider-kit-platform/src/loopback_callback.rs` | 213 | parse/46 | 1/2/0/0 | pure bounded callback parser. scans는 bounded security fields. |
| `crates/rust-provider-kit-platform/src/pkce.rs` | 43 | generate/21 | 0/0/0/0 | CSPRNG/SHA-256 effect adapter. |
| `crates/rust-provider-kit-platform/src/tests.rs` | 275 | loopback_callback_parser_accepts_only_exact_origin_path_host_and_state/71 | 0/0/12/4 | private platform seam tests. 유지. |
| `crates/rust-provider-kit-platform/tests/public_api.rs` | 26 | public_pkce_surface_matches_rfc_7636_s256/11 | 0/0/0/0 | public facade contract test. |
| `crates/rust-provider-kit-runtime/src/account_supervisor.rs` | 498 | register/75 | 1/0/19/14 | account admission/reconciliation/block/join authority; session effect와 분리됨. |
| `crates/rust-provider-kit-runtime/src/adapter.rs` | 43 | - | 0/0/0/0 | 실제 dialect variation port. |
| `crates/rust-provider-kit-runtime/src/adapters/anthropic.rs` | 760 | encode_request/204 | 2/4/2/0 | Anthropic/MiniMax/Z.AI wire state owner; partition loops라 선형. |
| `crates/rust-provider-kit-runtime/src/adapters/gemini.rs` | 707 | encode_request/217 | 2/4/2/0 | Gemini GenerateContent/thought lifecycle owner; 선형. |
| `crates/rust-provider-kit-runtime/src/adapters/mod.rs` | 9 | - | 0/0/0/0 | module wiring. |
| `crates/rust-provider-kit-runtime/src/adapters/openai_chat.rs` | 821 | encode_request/215 | 2/3/2/0 | OpenAI-compatible Chat schema owner; partition loops라 선형. |
| `crates/rust-provider-kit-runtime/src/adapters/openai_responses.rs` | 841 | consume/210 | 2/4/8/0 | OpenAI/Codex Responses schema·credential boundary; blocking I/O 격리됨. |
| `crates/rust-provider-kit-runtime/src/credential_contract.rs` | 94 | validate_lease/20 | 1/0/0/0 | record/material cross-validation choke. |
| `crates/rust-provider-kit-runtime/src/execution_session.rs` | 594 | open_and_consume/92 | 2/1/32/0 | 한 turn retry/SSE/cleanup/terminal owner; request clone은 O(1). |
| `crates/rust-provider-kit-runtime/src/execution_supervisor.rs` | 286 | execute/82 | 1/0/7/6 | request/account indices와 cancel/join. revoke O(k). |
| `crates/rust-provider-kit-runtime/src/http_transport.rs` | 407 | with_limits/33 | 1/0/5/0 | validated internal HTTP contract. reqwest effect와 분리됨. |
| `crates/rust-provider-kit-runtime/src/lib.rs` | 32 | - | 0/0/0/0 | Runtime facade 2종만 공개. |
| `crates/rust-provider-kit-runtime/src/oauth_replay.rs` | 68 | consume/15 | 0/0/0/0 | bounded HashSet+ring replay ledger, 평균 O(1). |
| `crates/rust-provider-kit-runtime/src/openrouter_oauth.rs` | 435 | validate_callback/69 | 1/0/4/0 | authorization/callback/exchange transaction. |
| `crates/rust-provider-kit-runtime/src/registration_session.rs` | 476 | cancellation_checkpoint_runs_after_state_producing_events_before_effects/58 | 2/0/13/0 | reducer effect interpreter와 compensation owner. |
| `crates/rust-provider-kit-runtime/src/registry.rs` | 130 | new/30 | 0/0/0/0 | 고정 10-provider registry, unknown fallback 없음. |
| `crates/rust-provider-kit-runtime/src/reqwest_transport.rs` | 201 | open/98 | 1/0/2/0 | network worker/backpressure/cancel/join effect. |
| `crates/rust-provider-kit-runtime/src/runtime.rs` | 582 | begin_scoped_account_control/50 | 1/0/21/14 | public facade와 global lifecycle/mutation fences. lock-held await 없음. |
| `crates/rust-provider-kit-runtime/src/secure_file.rs` | 81 | read/43 | 0/0/1/0 | descriptor-bound bounded file read + async blocking boundary. |
| `crates/rust-provider-kit-runtime/src/codex_version.rs` | 368 | resolve_executable_version/44 | 1/0/15/3 | managed metadata, executable qualification, single-flight process probe. |
| `crates/rust-provider-kit-runtime/src/sse.rs` | 210 | process_line/42 | 2/0/0/0 | one-pass bounded SSE parser; reconnect-only state 제거. |
| `crates/rust-provider-kit-runtime/src/in_memory_credential_store.rs` | 213 | stage/38 | 0/0/2/7 | process-lifetime credential store; ID-keyed lookup과 bounded test gate를 유지한다. |
| `crates/rust-provider-kit-runtime/src/tests.rs` | 1,823 | deepseek_kimi_and_zai_use_gajae_aligned_dialects/76 | 1/0/71/4 | private runtime/race/failure tests. production giant object 아님. |
| `crates/rust-provider-kit-runtime/src/wire.rs` | 558 | parse_model_catalog/52 | 1/0/0/0 | common parse/normalize/error mapping. I/O 없음. |
| `crates/rust-provider-kit-runtime/tests/public_api.rs` | 45 | oauth_registration_request_validates_and_redacts_sensitive_context/18 | 0/0/0/0 | facade/export/redaction public test. |

## 7. 남은 구조·성능 위험

1. Core가 완전히 순수하지는 않다. Public event stream과 `SystemProviderClock` effect가 Core에 남는다. 현 공개 타입 경로를 깨지 않고 옮길 이익이 작아 다음 major contract 때만 검토한다.
2. Adapter 요청 생성의 `ProviderJsonValue ↔ serde_json::Value` 변환은 O(n) 다중 pass와 allocation을 만든다. `[UNVERIFIED]` 실제 병목 여부는 profile 증거가 없다.
3. Secure file reader는 symlink와 descriptor identity/size 변화를 막지만 같은 inode·같은 길이의 동시 내용 변경까지 원자적 snapshot으로 보장하지는 않는다.
4. 실제 Provider 응답 크기·event 분포에 대한 production profile은 `[UNVERIFIED]`다.

## 8. 검증

- complexity inventory/document coverage: PASS
- front-shift forbidden scan: PASS
- Rust format/check/Clippy/test: PASS
- live Provider/macOS browser profile: `[UNVERIFIED]`
