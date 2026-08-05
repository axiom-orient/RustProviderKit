# Repository rules

- Use `scripts/verify.sh` as the only repository verification entrypoint.
- Use `scripts/clean.sh` for repository-local generated artifacts only.
- Do not create, modify, restore, or suggest GitHub Actions, CI/CD workflows,
  release automation, or any file under `.github/workflows/`.
- Do not add generated `REPORT`, audit, migration, compatibility, or legacy
  files. Keep durable behavior in Rust source, tests, the README, and the two
  canonical contract documents under `docs/`.
- Keep the public API fail-closed. Unsupported behavior must be rejected, not
  represented by a compatibility switch or silent fallback.
