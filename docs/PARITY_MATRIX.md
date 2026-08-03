# RustProviderKit ↔ SEMIProviderKit Parity Matrix

`RustProviderKit` is the Rust workspace for ARA. `SEMIProviderKit` is the Swift
package for SEMI. They expose corresponding provider-only contracts while
retaining separate package and product identities. The language runtime may
differ; state transitions, failure meaning, and ownership do not.

| Contract | Rust | Swift | Status |
| --- | --- | --- | --- |
| Package identity | `RustProviderKit`, `rust-provider-kit-*` | `SEMIProviderKit`, `SEMIProvider*` | distinct products; contract aligned |
| Primary subscription provider | `codex` / `CodexResponses` | `codex` / `codexResponses` | aligned |
| Credential boundary | `ProviderCredentialStore` | `ProviderCredentialStore` | aligned |
| Ephemeral store | `InMemoryProviderCredentialStore` | `InMemoryProviderCredentialStore` | aligned |
| Account state | pure stage → verify → activate → compensate | same reducer transition | aligned |
| Turn state | pure admission → stream → cleanup → terminal | same reducer transition | aligned |
| Concurrency shell | Tokio supervisors, bounded channels, cancellation tokens | actors, bounded async streams, task cancellation | language-native |
| OAuth platform effect | Tokio loopback/PKCE crate | AppKit/Network loopback and Security PKCE | language-native |
| Recovery | reconciliation, revoke fencing, typed recovery failure | same | aligned |
| Complexity policy | cursors, bounded input, ID indexes | same | aligned |

No agent planning, tool execution authority, durable run state, UI, or
cross-provider fallback belongs to either package.
