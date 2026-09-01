## Summary

Describe the user-visible behavior and the public-boundary impact.

## Validation

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `cargo test --locked --workspace --all-targets`
- [ ] `npm test`
- [ ] `python3 .github/scripts/public_boundary.py .`

## Data handling and compatibility

- [ ] The change does not add credentials, customer data, raw local paths, or private deployment topology.
- [ ] Any new collection or transmission of user data is documented in `docs/data-handling.md`.
- [ ] Existing npm executable aliases and MCP wire compatibility are preserved, or the change is explicitly marked breaking.

## Contribution certification

- [ ] Every commit includes a `Signed-off-by:` trailer (`git commit -s`).
