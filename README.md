# RustProviderKit

외부 Swift reference의 관찰 가능한 Provider 실행 계약을 Rust-native 구조로 구현한 Cargo workspace다. 호출자 소유 credential vault에서 계정을 등록·검증하고, 선택한 단일 Provider route에서 model turn을 실행하며, 결과를 bounded event stream으로 정규화한다.

현재 상태는 **MIT 공개 source candidate (`LOCAL_VERIFIED / EXTERNAL_UNVERIFIED`)**다. Rust 1.97.1에서 format, check, Clippy warnings-as-errors와 전체 workspace test를 통과했다. 실제 macOS browser lifecycle과 실제 credential을 사용하는 10개 Provider 호출은 검증하지 않았으므로 `release complete`로 표시하지 않는다.

## Workspace

| Crate | 책임 |
|---|---|
| `rust-provider-kit-core` | typed domain, 값 검증, account/execution pure reducer, bounded single-consumer stream |
| `rust-provider-kit-runtime` | 공개 facade, supervisor/session, credential port, HTTP/SSE, retry·cancel·timeout·cleanup, Provider dialect |
| `rust-provider-kit-platform` | RFC 7636 PKCE, browser adapter, bounded loopback OAuth effect와 pure callback parser |

의존 방향은 `rust-provider-kit-runtime → rust-provider-kit-core ← rust-provider-kit-platform`이다. Runtime과 Platform은 직접 의존하지 않는다.

## 공개 표면

- Core: 값 타입, reducer, event/terminal, credential·clock·authorization port
- Runtime: `ProviderRuntime`, `OpenRouterOAuthRegistrationRequest`
- Platform: `ProviderPkceGenerator`, `LoopbackAuthorizationSession`, `PreparedLoopbackAuthorization`

HTTP transport, Provider codec, supervisor/session, browser opener, callback parser와 테스트 주입 seam은 crate 내부 구현이다.

## 지원 Provider

`codex`, `openai`, `anthropic`, `gemini`, `openrouter`, `deepseek`, `qwen`, `kimi`, `zai`, `minimax`

알 수 없는 Provider는 `ProviderUnsupported`로 거부한다. Provider 간 자동 fallback은 없다.

## 사용 흐름

1. 호출자가 `ProviderCredentialStore`를 구현해 durable secret 저장을 소유한다.
2. `ProviderRuntime::new`에 vault를 전달한다.
3. `register` 또는 `register_open_router_oauth`로 stage→inspect→activate→read-back을 수행한다.
4. `inspect`, `models`로 account와 model catalog를 확인한다.
5. immutable `ProviderTurnRequest`를 `execute`에 전달하고 `ProviderEventStream`을 한 소비자가 읽는다.
6. 필요하면 `cancel_registration`, `cancel`, `revoke`, `shutdown`을 호출한다.

Tool 실행, Agent 계획, UI, conversation persistence, cross-provider fallback, credential 저장 구현은 호출자 책임이다.

## 로컬 검증

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
python3 scripts/validate_source.py .
python3 scripts/validate_swift_matrix.py .
python3 scripts/audit_complexity.py .
```

`Cargo.lock`은 source-candidate와 CI의 재현 가능한 검증을 위해 포함한다. 공개 라이브러리 소비자의 의존성 해석은 최종 애플리케이션이 소유한다.

## 증거 문서

- 실제 동작 참고 구현과 provider별 차용 판정: `docs/GAJAE_PROVIDER_PARITY.md`
- 세 저장소 비교와 차용 판정: `TRUNK_COMPARISON_REPORT.md`
- Swift↔Rust 기능 추적: `FEATURE_MATRIX.md`, `docs/SWIFT_CONTRACTS.json`
- 구조와 공개 계약: `docs/ARCHITECTURE.md`, `docs/INTERFACE_CONTRACT.md`
- 파일별 구조와 O(n²): `COMPLEXITY_AUDIT.md`
- 실행 명령 결과: `VALIDATION_REPORT.md`
- 완료 판정과 남은 gate: `COMPLETION_REPORT.md`, `VERIFY_LOCAL.md`

`docs/SWIFT_CONTRACTS.json`과 `FEATURE_MATRIX.md`의 외부 Swift reference는
`SEMIProviderKit`이다. 그 식별자는 검증 해시와 연결된 원본 증거이며,
RustProviderKit의 crate·모듈·공개 API 이름이 아니다. 현재 Provider ID와 표시명은
각각 `codex`, `Codex (ChatGPT subscription)`이다.

## 실패 정책

- credential·OAuth code·HTTP body를 Debug나 public failure에 노출하지 않는다.
- parse→normalize→validate→plan→apply 경계를 유지한다.
- HTTP/SSE·JSON·stream·loopback은 명시적 byte/node/queue/connection bound를 가진다.
- cancellation, timeout, overflow, malformed wire, worker 종료를 typed failure 또는 terminal event로 공개한다.
- credential reconciliation, revoke, registration과 direct account operation의 admission fence를 명시한다.
- production path에 silent fallback, fake success, placeholder, compatibility shim을 두지 않는다.

## 배포 권한

이 workspace는 [MIT License](LICENSE)로 배포한다. 공개 GitHub release 전에는 clean Git commit과 immutable semantic-version tag를 기준 revision으로 기록한다.

## Publication boundary

This repository publishes the Rust contracts and local verification surface. The
caller remains responsible for durable credential storage, host authorization, and
external TLS or application policy. `target/`, logs, local environment files, and
provider credentials are not source inputs. A clean verified commit is required
before creating a semantic-version tag or publishing a GitHub Release; the local
gate is the release evidence and GitHub Actions are intentionally out of scope.
