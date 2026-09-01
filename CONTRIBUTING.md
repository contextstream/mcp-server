# Contributing

Open an issue before large protocol or compatibility changes. Keep changes
scoped, add tests, and preserve existing wire schemas unless the change includes
an explicit migration.

Required local checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
npm test
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
```

Contributions must not include credentials, customer data, private repository
history, private operator material, or generated build outputs. Run Gitleaks
before submitting security-sensitive changes.

All commits must include a Developer Certificate of Origin sign-off:

```text
Signed-off-by: Your Name <you@example.com>
```

Add it with `git commit -s`. Pull requests are reviewed against correctness,
tests, protocol compatibility, privacy, security, and public-boundary checks.
