# RustProviderKit

Rust-native Cargo workspace for provider account registration, model lookup, and
bounded streaming turns. The caller owns durable credential storage; this
workspace owns typed contracts, provider adapters, cancellation, retry, timeout,
and cleanup.

## Workspace

| Crate | Responsibility |
|---|---|
| `rgxamk-native-provider` | Codex-only RGXAMK provider process; strict stdin/stdout boundary and action translation |
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

The `rgxamk-native-provider` leaf is a process adapter, not a runtime extension.
It accepts only explicit Codex arguments, an absolute `auth.json` reference, and
one bounded `rgx.agent.provider-request.v3` JSON document. It registers a private
ephemeral external-auth reference, requires an active read-back, executes one
Codex turn with a required strict `rgxamk_action` tool, and emits exactly one
`rgx.agent.provider-response.v2` line on success. Its explicit
`--operation-timeout-ms` covers registration, read-back, execution, and normal
shutdown; timeout cleanup is joined within a fixed 10-second reserve. It never
executes returned actions, discovers credentials from the environment, or
changes provider routes. One logical turn may make at most three same-route
attempts before visible output. HTTP failures and provider error events inside
an already-open stream share the same typed failure path. Short transient
429/5xx/transport failures use bounded Retry-After, OpenAI request/token/project reset
headers, or exponential backoff; usage-window exhaustion and delays over 60
seconds return immediately with reset evidence, while auth, billing, or
insufficient quota returns account-action evidence. Neither path holds the
process in a long sleep.

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

- Credential material and OAuth codes are redacted from public errors and `Debug`. Provider error bodies are never included in public errors; only the HTTP status is surfaced.
- Inputs, JSON, HTTP, SSE, streams, and loopback callbacks have explicit bounds.
- Cancellation, timeout, overflow, malformed wire, and worker failure become typed failures or terminal events.
- Unsupported fallback, compatibility switches, migration paths, fake success, and silent recovery are not part of the public contract.

The workspace is distributed under the [MIT License](LICENSE).

## Publication boundary

This repository publishes the Rust contracts and local verification surface. The
caller remains responsible for durable credential storage, host authorization, and
external TLS or application policy. `target/`, logs, local environment files, and
provider credentials are not source inputs. A clean verified commit is required
before creating a semantic-version tag or publishing a GitHub Release; the local
gate is the release evidence and GitHub Actions are intentionally out of scope.
