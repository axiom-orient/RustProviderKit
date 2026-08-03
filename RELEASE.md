# Release policy

`RustProviderKit` is the sole source repository for the three
`rust-provider-kit-*` crates. A release is an immutable Git tag on a clean,
locally verified commit.

Consumers use the exact Git tag and Cargo.lock-resolved revision. They do not
copy source into `vendor/`, use a symbolic link, or use an unpinned branch.

Before creating a release tag, run:

```bash
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets --all-features
python3 scripts/validate_source.py .
python3 scripts/validate_swift_matrix.py .
python3 scripts/audit_complexity.py .
```

The MIT license permits source distribution. Live provider and system-browser
verification remain opt-in evidence and must not be represented by a source
tag alone.
