# Gajae provider parity

## 결정

`/Users/ax/Downloads/gajae-code-main/packages/ai`는 실제 동작 참고 구현이고,
RustProviderKit은 Rust 계약과 배포 경계다. TypeScript runtime, SDK, agent, auth storage를
복사하지 않는다. provider별 endpoint, 인증 헤더, request dialect, reasoning/tool-history,
stream normalization만 RustProviderKit adapter 안에서 다시 구현한다.

Codex ChatGPT 구독 경로는 이 비교의 적용 대상이 아니며 기존
`OpenAiResponsesKind::Codex` 구현을 유지한다.

## 차용 매트릭스

| Provider | Gajae 근거 | RustProviderKit 적용 | 판정 |
|---|---|---|---|
| OpenAI API | `openai-responses.ts` | 기존 Responses request/stream 유지 | 유지 |
| Anthropic | `anthropic.ts` | `x-api-key`, `anthropic-version`, Messages 유지 | 유지 |
| Gemini | `google.ts`, `google-shared.ts` | Interactions 제거, public `v1beta/models/{model}:streamGenerateContent`와 GenerateContent body/stream/native thought-signature replay 적용 | 교체 |
| OpenRouter | `openai-completions.ts`, compat | streaming usage, nested reasoning, routing policy 유지 | 보강 |
| DeepSeek | OpenAI compat detector/descriptors | `max_tokens`, thinking toggle, effort mapping, forced tool-choice fail-closed, reasoning history 적용 | 보강 |
| Qwen | Qwen Portal descriptor/compat | `https://portal.qwen.ai/v1`, coalesced system, `enable_thinking` 적용; explicit regional endpoint override 유지 | 교체 |
| Kimi | Kimi Code model manager/compat | `https://api.kimi.com/coding/v1`, KimiCLI headers, binary thinking, `max_completion_tokens` 적용 | 교체 |
| Z.AI | `special.ts`, Anthropic provider | OpenAI chat 제거, `https://api.z.ai/api/anthropic/v1/messages`의 Anthropic Messages 적용 | 교체 |
| MiniMax | Anthropic descriptor | 기존 Anthropic-compatible Messages 경로 유지 | 유지 |

## 경계

- Core는 provider wire를 모른다. identifier, message, tool, native opaque state만 소유한다.
- Runtime adapter가 endpoint, header, JSON/SSE dialect와 provider별 실패를 소유한다.
- Agent/Arc/ARA는 API body나 provider SDK에 의존하지 않는다.
- portable text/tool history는 항상 유지한다. Gemini thought signature와 OpenAI-compatible
  reasoning field는 같은 route에만 재사용하는 `ProviderNativeState`로 격리한다.
- provider 간 fallback과 secret 저장은 추가하지 않는다.

## 참고 소스 fingerprint

2026-08-03 로컬 참고본의 SHA-256:

| 파일 | SHA-256 |
|---|---|
| `providers/openai-responses.ts` | `d93e825b4f19045f7fe16740d5f9a1a1cecf53f67e4c2b2903d3f45d2ebe9de4` |
| `providers/anthropic.ts` | `22a08bd98d6967b3cd0d1157b103b5641f974a8b46fb38762477eee50c7abb94` |
| `providers/google.ts` | `552e1d3638a617448abdf27bf2a0d0d36f973be17a742cd5f04b9cc63b498144` |
| `providers/google-shared.ts` | `ea30c6dee33159f378841a37d540e6a3d86f4652be2452a03011b4a28a73ba83` |
| `providers/openai-completions.ts` | `58800719be8c6320adcf720345d50772893574ecae683ce517cc550f6d1175ce` |
| `providers/openai-completions-compat.ts` | `3c4d7ae15535aa77af41939a0711a5340351112781c30484ad2f37f3b8f4996b` |
| `provider-models/openai-compat.ts` | `01a579274a7c21b89316394a104c838c0b58c52ad997718e175b46231aad1ddf` |
| `provider-models/special.ts` | `23bc7c2c0d613e1fb909b6b8e52e6c25f5db251e63304f89499df242081b357f` |

참고 프로젝트 smoke는 workspace link `@gajae-code/utils/postmortem` 부재로 현재 checkout에서
실행되지 않았다. 이 환경 실패를 provider 동작 성공 증거로 사용하지 않는다. 적용된 Rust
wire 계약은 RustProviderKit의 deterministic request/decoder 테스트와 Arc·ARA 통합 검증으로
판정한다.
