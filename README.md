# RustProviderKit

Rust-native Cargo workspace for provider account registration, model lookup, and
bounded streaming turns. The caller owns durable credential storage; this
workspace owns typed contracts, provider adapters, cancellation, retry, timeout,
and cleanup.

## Workspace

| Crate | Responsibility |
|---|---|
| `rust-provider-kit-core` | typed values, validation, reducers, events, and credential ports |
| `rust-provider-kit-runtime` | runtime facade, HTTP/SSE, provider adapters, and lifecycle supervision |
| `rust-provider-kit-platform` | RFC 7636 PKCE and bounded loopback OAuth effects |

The dependency direction is `runtime → core ← platform`; runtime and platform do
not depend on each other.

## Public surface

- Core: values, errors, events, reducers, and credential/clock/authorization ports
- Runtime: `ProviderRuntime`, `ProviderRuntimeOptions`, and `OpenRouterOAuthRegistrationRequest`
- Platform: `ProviderPkceGenerator`, `LoopbackAuthorizationSession`, and `PreparedLoopbackAuthorization`

HTTP transport, provider codecs, supervisors, sessions, callback parsing, and
test seams remain private.

Supported providers are `codex`, `openai`, `anthropic`, `gemini`, `openrouter`,
`deepseek`, `qwen`, `kimi`, `zai`, and `minimax`. Unknown providers and all
provider fallback are rejected.

## Runtime options

`ProviderRuntime::new` keeps the default behavior. `ProviderRuntime::with_options`
is the explicit construction path for process-wide protocol settings, including
`ProviderRuntimeOptions::codex_client_version`. Codex version precedence is
explicit configuration, then `ARA_PROVIDER_KIT_CODEX_CLIENT_VERSION`, then the
built-in version implemented by this kit. Invalid explicit values fail during
construction. The provider kit does not inspect a host Codex executable, `PATH`,
or process output.

## Local commands

```bash
./scripts/verify.sh
./scripts/clean.sh
```

`verify.sh` is the canonical local gate: format, locked workspace check,
warnings-as-errors Clippy, tests, repository hygiene, and the no-GitHub-workflow
guard. `clean.sh` removes only repository-local generated artifacts. `Cargo.lock`
is retained for reproducible local verification.

GitHub Actions, CI/CD workflows, and release automation are intentionally not
part of this repository. Do not add files under `.github/workflows/`.

## Documentation

- [Architecture](docs/ARCHITECTURE.md): ownership and failure boundaries
- [Interface contract](docs/INTERFACE_CONTRACT.md): public inputs, outputs, and provider routes

## Security and failure policy

- Credential material, OAuth codes, and provider bodies are redacted from public errors and `Debug`.
- Inputs, JSON, HTTP, SSE, streams, and loopback callbacks have explicit bounds.
- Cancellation, timeout, overflow, malformed wire, and worker failure become typed failures or terminal events.
- Unsupported fallback, compatibility switches, migration paths, fake success, and silent recovery are not part of the public contract.

The workspace is distributed under the [MIT License](LICENSE).
