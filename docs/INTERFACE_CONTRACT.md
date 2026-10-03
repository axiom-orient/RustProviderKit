# Interface Contract

## Public facade

`rust_provider_kit_runtime::ProviderRuntime`

| Method | 관찰 가능한 계약 |
|---|---|
| `new` | caller-owned credential vault를 받아 기본 `ProviderRuntimeOptions`와 built-in registry, HTTP transport, system clock을 구성한다. secret의 durable ownership은 가져오지 않는다. |
| `with_options` | 명시적 `ProviderRuntimeOptions`로 runtime을 구성한다. Codex의 명시적 client version은 구성 시 검증되며, `explicit config > ARA_PROVIDER_KIT_CODEX_CLIENT_VERSION > built-in` 순서를 따른다. |
| `providers` | built-in 10 Provider를 deterministic order로 반환한다. |
| `accounts` | validated account summary를 deterministic order로 반환한다. |
| `reconcile_credentials` | staged record를 제거하고 실패는 recovery issue로 공개한다. registration/revoke와 상호 배타적인 admission fence다. |
| `register` | stage→inspect→activate→read-back 또는 compensation의 account stream을 반환한다. reconciliation/revoke 중에는 failed stream으로 닫힌다. |
| `cancel_registration` | registration cancellation을 요청하고 worker 완료를 join한다. |
| `revoke` | 신규 account/turn admission을 막고 관련 work를 cancel/join한 뒤 credential을 제거한다. 실패 시 fence를 복원한다. |
| `register_open_router_oauth` | PKCE state를 한 번 소비하고 callback query를 검증해 key exchange 후 일반 registration으로 연결한다. authorization 중에도 registration fence를 유지한다. |
| `inspect` | active lease identity/source를 검증하고 provider-native inspection을 수행한다. |
| `models` | provider catalog를 strict parse하고 deterministic model list로 정규화한다. |
| `execute` | 한 immutable request와 한 route를 admit하고 bounded single-consumer event stream을 반환한다. |
| `cancel` | request cancellation을 요청하고 cleanup을 join한다. |
| `shutdown` | 신규 work를 거부하고 admitted control/session을 cancel/join하며 concurrent caller가 한 lifecycle을 관찰한다. |

그 외 Runtime 구현 타입은 공개 계약이 아니다.

## RGXAMK native provider leaf

`rgxamk-native-provider` is a separate binary crate and does not change the
runtime facade. Its argv is explicit and complete:

```text
--account-id ID --model MODEL --auth-file ABSOLUTE_PATH
--codex-client-version VERSION --operation-timeout-ms MILLISECONDS
--upstream-max-response-bytes BYTES
[--failure-diagnostics v1]
```

Unknown, missing, duplicate, relative-path, and out-of-bound arguments fail
closed. The binary does not inspect environment credentials, model settings,
installed Codex clients, or fallback routes. It reads exactly one bounded
`rgx.agent.provider-request.v3` JSON object with denied unknown top-level fields.
The request capabilities determine one strict action-object schema for exactly
one named tool, `rgxamk_action`. With multiple negotiated capabilities, Codex
receives a flat strict object: `type` is an enum of the negotiated kinds, every
non-type field contributed by those kinds is required and nullable, and
`additionalProperties` is false. After the selected type is known, only null
fields belonging to another negotiated kind are removed. Unknown or
unnegotiated fields (including nulls) and non-null inactive fields are rejected.
With one capability, only that kind's strict object is accepted. The process
rejects v1, text deltas, wrong or multiple tools, failed/cancelled/incomplete
terminals, malformed or oversized arguments, and capability violations. It
never executes actions. On success it writes exactly one newline-terminated
`rgx.agent.provider-response.v2` JSON line. Without `--failure-diagnostics`,
failure writes exactly one legacy stable `code/message` line to stderr and no
stdout. With `--failure-diagnostics v1`, provider failures write that same
stable line followed by exactly one line prefixed
`rgxamk-native-provider diagnostic-v1 `; its compact JSON is at most 2 KiB and
contains only the typed failure code/status, bounded Retry-After, body
byte-count/SHA-256, validated request ID, allowlisted remote error type/code,
provider retry direction, an optional Unix-second reset time, and bounded
numeric Codex rate-limit fields. `retry_disposition` is one of `retry_soon`,
`wait_until_reset`, `user_action`, or `do_not_retry`. It never contains raw body,
message, headers, credentials, account IDs, paths, or request payloads.

The leaf registers `codex` with an absolute `ExternalAuthFile` reference in a
private ephemeral credential store, requires the successful active terminal and
lease read-back, and sets `maximum_retry_attempts = 3` (one initial attempt and
at most two retries). Retry stays on the exact immutable route and is allowed
only before visible output. `usage_limit_reached`, `rate_limit_reached`,
`insufficient_quota`, and `quota_exceeded` never auto-retry; a reset delay over
60 seconds also returns immediately. `Retry-After` is authoritative. If it is
absent, validated OpenAI request, token, and project-token reset duration
headers are a fallback. A valid
`x-should-retry: false` stops retry, while `true` may enable a bounded retry for
an otherwise terminal HTTP response. The same typed policy is applied to
provider error events received after a successful HTTP stream open; visible
output still permanently disables retry. Its
`--operation-timeout-ms` is one total work deadline for registration, active
lease read-back, the turn, and normal runtime shutdown. The leaf derives the
inner `ProviderTurnRequest` timeout from the remaining operation budget; it is
always positive and never exceeds that remaining budget. If the operation
deadline expires, runtime shutdown/cancellation is joined within a fixed
10,000ms cleanup reserve. The 100,000ms maximum operation setting plus that
reserve stays below RGXAMK's 120-second outer process timeout. Runtime
shutdown/join completes before the success line.

## Platform surface

| Type | 계약 |
|---|---|
| `ProviderPkceGenerator` | bounded verifier/state를 생성하고 RFC 7636 S256 challenge를 만든다. |
| `LoopbackAuthorizationSession` | listener를 먼저 bind하고, bounded callback server와 browser lifecycle을 소유한다. |
| `PreparedLoopbackAuthorization` | 준비된 session과 정확한 callback URL을 함께 반환한다. |

## Input

- identifier, label, tool/schema, continuation, JSON은 typed·bounded다.
- native leaf operation timeout `1,000...100,000ms` and response bytes are
  validated at the process boundary; core request constraints continue to
  enforce their own timeout, response, retry, and output-token bounds.
- Provider/account/model 선택은 독립 값이며 lease의 account/provider/source/active state와 일치해야 한다.
- HTTP request는 absolute HTTPS, no userinfo/fragment, valid header여야 한다.
- Account endpoint는 최대 16개의 non-sensitive integration header를 가질 수 있다. 인증, credential, `accept`, content/host/connection/transfer header 재정의는 거부한다.
- JSON request는 default `Accept: application/json`, body가 있는 adapter는 `Content-Type: application/json`을 가진다.
- execution timeout은 typed HTTP request와 reqwest까지 전달한다.
- loopback callback은 exact `127.0.0.1:port/path`, Host, state, percent encoding을 검증한다.
- Codex auth reference만 regular non-symlink·bounded read로 읽는다. client version은 explicit config, 환경 override, built-in constant로 결정하며 Codex executable·`PATH`·process probe는 수행하지 않는다.

## Output와 순서

```text
Started
→ zero or more ReasoningDelta / TextDelta / ToolCall
→ exactly one Terminal(Completed | Failed | Cancelled)
```

- adjacent text와 reasoning은 같은 종류·순서 보존 조건에서만 coalesce하며 서로 합쳐지지 않는다.
- terminal slot은 backlog와 별도로 예약한다.
- terminal은 transport termination과 cleanup 뒤에 공개한다.
- visible output 이후에는 retry하지 않는다.
- automatic retry는 같은 route에서 총 세 번 이하이며 전체 request deadline을
  새로 시작하지 않는다. `retry-after-ms` 또는 Retry-After가 1..=60초면 최소
  대기로 존중하고, 값이 없으면 request ID로 분산된 bounded exponential backoff를
  사용한다.
- 장기 quota/reset, billing/spend-control, credential recovery는 sleeping loop나
  provider fallback으로 숨기지 않고 typed terminal로 반환한다.
- request/account admission ID는 terminal 공개 직전에 해제한다.
- `ProviderTurnRequest` clone은 immutable shared storage를 사용하지만 serde·Debug·accessor의 공개 의미는 동일하다.
- SSE reconnect를 수행하지 않으므로 wire `id`와 `retry`는 파싱 결과에 보존하지 않는다.

`ProviderCompletion.native_state`는 provider가 명시적으로 부여한 opaque continuation material이다. OpenAI Responses output은 `openai.responses.output.v1`, OpenAI-compatible reasoning은 `openai.chat.reasoning.v1`, Gemini parts/thought signature는 `google.generate-content.parts.v1`로 보관한다. 각 payload에는 생성한 `provider`와 `model`을 함께 봉인하며 adapter는 둘 중 하나라도 다르면 재입력을 거부한다. portable message/tool history는 항상 별도로 유지한다.

## Provider routing

| Provider | Dialect | 기본 route |
|---|---|---|
| Codex | OpenAI Responses | `https://chatgpt.com/backend-api/codex/responses` |
| OpenAI | OpenAI Responses | `https://api.openai.com/v1/responses` |
| Anthropic | Messages | `https://api.anthropic.com/v1/messages` |
| MiniMax | Messages | execution `https://api.minimax.io/anthropic/v1/messages`, models `https://api.minimax.io/v1/models` |
| Gemini | GenerateContent | `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent` |
| OpenRouter | Chat Completions | `https://openrouter.ai/api/v1/chat/completions` |
| DeepSeek | Chat Completions | `https://api.deepseek.com/chat/completions` |
| Qwen | Chat Completions | default `https://portal.qwen.ai/v1/chat/completions`; explicit regional endpoint override 가능 |
| Kimi | Chat Completions | `https://api.kimi.com/coding/v1/chat/completions` |
| Z.AI | Messages | `https://api.z.ai/api/anthropic/v1/messages` |

Provider endpoint fallback은 지원하지 않는다. OpenRouter 요청도 fallback을
명시적으로 `false`로 고정하며, 공개 request policy에는 이를 바꾸는 필드가
없다. cross-provider fallback과 migration/compatibility path도 없다.

## Error

`ProviderFailureCode`는 invalid request, unsupported provider/capability, account/auth/permission, transport/server/rate-limit, malformed/oversized response, backpressure, cancellation, timeout, recovery requirement, internal invariant를 구분한다.

- reqwest timeout은 `TimedOut`으로 분류한다.
- 429는 동일하지 않다. 짧은 rolling-window 제한은 `retry_soon`,
  `usage_limit_reached`/`rate_limit_reached` 또는 확인된 긴 reset은
  `wait_until_reset`, `insufficient_quota`/`quota_exceeded`와
  auth/billing/permission은 `user_action`으로 구분한다.
- Codex의 `resets_at`/`reset_at` 또는 bounded `resets_in_seconds` body field와
  `used-percent`/`window-minutes` header는
  정수·범위 검증 후에만 evidence로 보존한다. reset은 Unix seconds다.
- OpenAI의 request/token/project-token reset duration은 `Retry-After`가 없을 때만 사용하며
  숫자 seconds 또는 bounded `ms`/`s`/`m`/`h` 조합만 수용한다.
- HTTP 2xx 뒤 stream error도 같은 redacted evidence와 retry gate를 사용한다.
- provider error body는 public error에 절대 포함하지 않고 HTTP status만 공개한다. OAuth secret은 redacted message로 정규화한다.
- Codex route의 4xx에는 선언된 client version에 관한 중립적 context만 추가하며, 다른 4xx 원인을 단정하지 않는다.
- malformed optional wire field를 누락으로 보정하지 않는다.
- worker panic/abort와 producer 소멸은 영구 대기 대신 explicit terminal 또는 completion으로 수렴한다.
- credential mutation fence 충돌은 조용히 대기하거나 경쟁하지 않고 typed `InvalidRequest`/`AccountUnavailable`로 공개한다.

## Non-goals

Agent orchestration, tool 실행, UI, conversation/run persistence, credential vault 구현, cross-provider fallback은 이 workspace의 책임이 아니다.
