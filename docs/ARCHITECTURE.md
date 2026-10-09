# Architecture

## 1. 제품과 소유권 경계

```text
caller-owned UI / agent / tool executor / durable credential vault
                              │
                              ▼
                       ProviderRuntime
               lifecycle + mutation/admission fences
                  ┌───────────┴───────────┐
                  ▼                       ▼
         account supervisor      execution supervisor
                  │                       │
         registration session      execution session
                  └───────────┬───────────┘
                              ▼
              provider adapter → typed HTTP/SSE → reqwest

rust-provider-kit-platform → rust-provider-kit-core ← rust-provider-kit-runtime
 PKCE + loopback         values/reducers/ports      effects
```

Cargo 의존 방향은 `runtime → core ← platform`이다. Runtime과 Platform은 직접 의존하지 않는다. Tool 실행, Agent planning, UI, 대화 저장, cross-provider fallback과 durable vault 구현은 라이브러리 밖 호출자 책임이다.

## 2. 공개 표면

| Crate | 공개 계약 | 내부 구현 |
|---|---|---|
| `rgxamk-native-provider` | RGXAMK Codex provider process boundary | strict argv/JSON adapter, private ephemeral credential store, event accumulator |
| Core | 값 타입, 오류·이벤트·terminal, reducer, vault/clock/authorization port, bounded stream | mailbox state와 기본 clock |
| Runtime | `ProviderRuntime`, `OpenRouterOAuthRegistrationRequest` | adapters, HTTP/SSE, supervisors, sessions, registry, OAuth broker |
| Platform | PKCE generator, loopback session/prepared request | browser opener, callback parser/scanner, socket lifecycle |

Production module은 crate-root 내부 re-export hub를 경유하지 않고 소유 모듈을 직접 참조한다. 내부 characterization test는 crate unit test, 외부 계약 test는 public API integration test로 분리한다.

The native provider leaf is deliberately outside runtime ownership. It consumes the
product's current `rgx.agent.provider-request.v3` envelope, builds one immutable
Codex `ProviderTurnRequest`, and accepts one named `rgxamk_action` tool call only.
Registration owns a private process-lifetime credential store and checks the active
lease after the runtime's stage→verify→activate→read-back terminal. The leaf then
shuts the runtime down and joins cleanup before writing its single success line;
actions remain caller-owned and are never executed here. The leaf's
`--operation-timeout-ms` is a total registration/read-back/execute/normal-shutdown
deadline. A turn timeout is derived from the remaining budget, and a fixed
10,000ms shutdown reserve is used only when the work deadline expires.

## 3. Core의 실제 순도 경계

Core 전체가 순수한 것은 아니다. 정확한 경계는 다음과 같다.

- 순수: identifier/value validation, account reducer, execution reducer, bounded JSON value
- Port: credential vault, clock, authorization session trait
- 동시성 계약: public account/turn event stream mailbox
- 작은 기본 effect: `SystemProviderClock`

Reducer는 실제 상태 전이와 복구 의미가 있는 두 영역에만 적용한다.

- account: stage→verify→activate→durable read-back, compensation, recovery-required
- execution: idle→opening→streaming→cleanup→terminal, visible-output commit과 retry gate

Provider codec, 값 변환, catalog sort에는 reducer/actor를 적용하지 않는다. Public stream과 `SystemProviderClock`을 별도 crate로 옮기는 것은 기존 Core 공개 계약을 깨는 구조 변경이라 이번 범위에서는 유지했다.

## 4. 입력→출력 데이터 경로

```text
caller input
→ typed constructor / serde validation
→ fixed provider-account-model selection
→ active credential lease + record/material cross-check
→ provider-native request encoding
→ validated immutable HTTP request
→ bounded HTTP worker and chunk queue
→ incremental SSE parser
→ stateful provider dialect decoder
→ declared tool-name and named-choice scope check
→ explicit ProviderExecutionEvent
→ pure reducer plan
→ effect interpreter
→ bounded ProviderTurnEvent stream
→ transport cleanup/join
→ exactly one terminal
```

`ProviderTurnRequest`는 private `Arc` 저장소를 사용한다. 공개 accessor·serde·Debug 계약은 유지하면서 reducer의 transactional `state.clone()`이 request 크기와 무관한 O(1)이 되도록 한다.

## 5. Account 경계

Credential admission is authentication-only: direct API-key material is not a
core value and cannot be registered. OAuth-derived material is accepted through
the caller-owned credential port. Codex subscription access is represented by an
external `auth.json` reference and is registered through
`register_codex_subscription`; OpenRouter's PKCE flow stores only its
OAuth-derived credential. External auth-file references are Codex-only, and the
runtime never copies or persists the referenced file.

- `account_supervisor.rs`: registration inventory/admission, reconciliation fence, account block, cancel/join, inspect/models
- `registration_session.rs`: 한 registration의 reducer event→effect 해석과 compensation
- `credential_contract.rs`: vault record와 lease material의 fail-closed 교차 검증
- `runtime.rs`: 공개 operation 사이의 global reconciliation/revocation/admission 순서

Credential reconciliation과 registration은 양방향 admission fence다. OAuth authorization이 끝나기 전에도 registration control을 유지한다. Revoke는 해당 account의 direct operation이 끝날 때까지 기다린 뒤 registration과 execution을 block/cancel/join하고 credential을 제거한다. 실패나 caller cancellation 시 RAII cleanup이 fence를 복원한다.

## 6. Execution 경계

- `execution_supervisor.rs`: request/account index, account block, cancel/join, shutdown
- `execution_session.rs`: 한 immutable route의 retry, HTTP/SSE consume, event publication, cleanup, terminal

`request_index`는 중복 request admission을 O(1)로 막고, `account_index`는 revoke가 전체 session을 훑지 않고 해당 account의 k개 session만 O(k)에 취소하도록 한다. Session entry는 worker cleanup까지 유지하되 request ID는 terminal 관찰 직전에 해제한다.

## 7. HTTP·SSE·Provider dialect

- `http_transport.rs`: HTTPS/header/timeout/body/chunk bound를 검증한 내부 contract
- `reqwest_transport.rs`: redirect-disabled client, per-request timeout, bounded chunk queue, cancel/termination control
- `sse.rs`: wire bytes를 한 번 스캔하는 bounded incremental parser
- `wire.rs`: JSON conversion, model catalog, usage, HTTP failure normalization
- `adapters/*`: 실제 Provider dialect 차이

SSE `id`와 `retry`는 reconnect를 하지 않는 one-shot transport에서 소비자가 없으므로 저장하지 않는다. Retry body는 reference-counted `Bytes`를 사용한다. Codex auth reference만 regular non-symlink·bounded read로 읽는다. client version은 explicit config, 환경 override, built-in constant로 결정하며 Codex executable·`PATH`·process probe는 수행하지 않는다.

지원 dialect:

- OpenAI Responses: Codex, OpenAI
- Gemini GenerateContent: Gemini
- OpenAI-compatible Chat: OpenRouter, DeepSeek, Qwen, Kimi
- Anthropic-compatible Messages: Anthropic, MiniMax, Z.AI

Unknown Provider fallback은 없고 MiniMax execution/model catalog base는 분리한다.

## 8. Platform 경계

- `pkce.rs`: OS CSPRNG + SHA-256 RFC 7636 S256
- `loopback_callback.rs`: bounded bytes→validated callback URL pure parser
- `loopback.rs`: listener bind, browser, connection ownership, timeout/cancellation

Listener를 먼저 bind한 뒤 browser를 연다. request bytes, connection count, header duration, Host/origin/path/state를 제한한다.

## 9. 동시성 원칙

- 짧은 owner state만 `parking_lot::Mutex`로 보호하고 lock을 잡은 채 await하지 않는다.
- cancellation은 `CancellationToken`, 완료·lifecycle은 stateful `watch`를 사용한다.
- HTTP chunk는 bounded `mpsc`, public event는 상태를 가진 single-consumer mailbox다.
- runtime-owned worker는 caller future와 분리되고 panic/abort에도 terminal 또는 completion을 남긴다.
- sync filesystem/process metadata는 async executor에서 직접 실행하지 않는다.
- public domain에 `Arc<Mutex<_>>`를 노출하지 않는다.

## 10. 실패와 복구

| 실패 | 공개 결과 | 복구 경계 |
|---|---|---|
| stage/verify/activate/read-back 실패 | typed account failure | staged/active credential compensation |
| compensation 실패 | `CredentialRecoveryRequired` | 호출자에게 명시적 수동 복구 요구 |
| HTTP malformed/oversize/backpressure | failed terminal | transport cancel + join |
| timeout | `TimedOut` failure | cancellation과 의미 분리 |
| visible output 전 transient HTTP/stream failure | bounded same-route retry | route 변경 없음 |
| 장기 usage-window 429 또는 reset delay > 60초 | `wait_until_reset` typed failure | process가 장시간 sleep하거나 hot-loop하지 않음 |
| auth/billing/permission/insufficient quota | `user_action` typed failure | 자동 logout·credential·plan·credit·결제 side effect 없음 |
| visible output 후 failure | terminal failure | 중복 출력 방지를 위해 retry 없음 |
| revoke 실패 | typed failure | admission block을 복원해 기존 account 사용 가능 |
| worker panic/abort | internal-invariant terminal | completion watch와 emergency sink |

HTTP status가 성공이어도 Responses/Chat/Messages/GenerateContent stream 안의 error event가
429·usage limit·overload를 운반할 수 있습니다. 각 decoder는 원문 message를 공개하지 않고
status, allowlisted type/code, body digest, request ID, reset 및 bounded rate-limit 수치만 공통
failure evidence로 바꿉니다. `Retry-After`가 없을 때만 OpenAI
`x-ratelimit-reset-requests`/`x-ratelimit-reset-tokens`/project-token reset을 사용하며, exhausted dimension이
명시되면 그 dimension만, 없으면 관찰된 reset 중 가장 긴 값을 선택합니다.

## 11. 의도적으로 유지한 큰 authority

`model.rs`는 공개 typed domain+validation+serde authority다. `execution_session.rs`는 한 turn의 transport/retry/cleanup/terminal ordering authority다. 물리적 크기만으로 분리하면 불변식의 owner가 흩어진다.
