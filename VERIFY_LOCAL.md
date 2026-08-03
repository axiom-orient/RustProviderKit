# VERIFY_LOCAL

로컬 Rust gate는 현재 checkout에서 통과했다. 이 문서는 재현 명령과 아직 닫히지 않은 external/product-flow proof를 기록한다.

## 1. 재현 가능한 local gate

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --no-default-features --locked
python3 scripts/validate_source.py . --require-package
python3 scripts/validate_swift_matrix.py .
python3 scripts/audit_complexity.py .
```

성공 기준: 모두 exit 0, Rust tests 85, matrix 92/92, complexity audit의 현재 측정값.

## 2. Direct-test coverage gap

`FEATURE_MATRIX.md`에서 `COMPILE_PASS; COVERAGE_GAP`인 34행을 위험 순서대로 direct scenario test로 전환한다.

1. shutdown/execute/cancel/revoke ordering
2. registration cancellation과 compensation timing
3. runtime streaming/HTTP failure/backpressure
4. 모든 dialect의 explicit tool/reasoning/output-limit encoding
5. OAuth exchange/replay callback lifecycle

Mock의 성공만으로 production proof를 주장하지 않는다. deterministic transport/vault fixture로 실제 state와 terminal ordering을 관찰한다.

## 3. macOS system-browser OAuth

사용자가 승인한 macOS test consumer에서 `LoopbackAuthorizationSession::prepare`와 OpenRouter OAuth를 실행한다.

성공 기준:

- listener bind 후 browser open
- wrong/duplicate state, wrong Host/path/origin, malformed percent encoding을 exchange 전에 거부
- cancellation/timeout 후 listener와 connection task 정리
- 정상 callback을 한 번만 소비하고 credential activation read-back 성공

현재 parser/listener mock tests는 PASS지만 실제 browser application은 `[UNVERIFIED]`다.

## 4. Live Provider

실제 credential은 opt-in으로만 사용한다. 각 Provider에서 다음 최소 flow를 수행한다.

```text
register → account ready/read-back
→ inspect → models
→ text stream + usage
→ provider가 지원하면 tool call
→ cancel/timeout/failure redaction
→ revoke → credential absent
```

대상: `codex`, `openai`, `anthropic`, `gemini`, `openrouter`, `deepseek`, `qwen`, `kimi`, `zai`, `minimax`.

실제 body/header/token을 report에 기록하지 않는다. API drift가 확인되면 해당 adapter fixture와 regression test를 먼저 추가한다.

## 5. Production profile

Representative long prompt, 512 messages, 128 tools, large SSE delta에서 다음을 측정한다.

- request clone allocation이 payload와 무관한지
- Core JSON↔serde JSON allocation/CPU 비중
- event/SSE queue high-water mark와 backpressure
- account-scoped revoke latency O(k)

측정 전에는 JSON validation pass나 queue bound를 추측으로 제거하지 않는다.

## 6. Source archive

Source archive 생성은 parent ARA의 `scripts/package_source.py`가 담당한다.
ProviderKit 자체는 Rust/Swift 구조·계약 검증만 수행하며 생성형 source
checksum 파일을 만들지 않는다.
