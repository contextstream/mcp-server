# Release process

Releases run end to end in Depot CI (`.depot/workflows/release.yml`), with no person in the loop. The logic is in
`scripts/release/mcp_release.py`; the builds are `scripts/release/build.sh` and the gate is `scripts/release/gate.sh`.
GitHub Actions does not run releases; the old `.github/workflows/release.yml` is kept for its tests only.

## Cutting a release

Merge a "prepare MCP X.Y.Z" change to `main`: the same stable version in `Cargo.toml`, `package.json` and
`server.json`, and a `## X.Y.Z` section in `CHANGELOG.md` (it becomes the GitHub release notes). That push releases it:

1. **plan** reads the version at the pushed commit and what every public channel holds for it. A version that is
   already out everywhere ships nothing, so ordinary pushes cost one small job.
2. **gate** (two parts side by side) runs fmt, clippy, the workspace tests, the account-connection smoke, cargo
   audit, cargo deny, the release contract, the public boundary and every script and launcher test.
3. **build** makes the six binaries in parallel: Linux x64, x64 with remote acceleration, arm64 (native arm runner),
   Windows (cargo-xwin) and darwin arm64/x64 (cargo-zigbuild with `scripts/release/macos-link-stubs`). Linux and arm64
   binaries run `--version` and stay under the glibc 2.39 floor, Windows runs under Wine, and the darwin binaries are
   checked structurally (`mcp_release.py macho-check`: linked libraries, minimum macOS, signature).
4. **publish**, in order: SBOM, `version.json` and `checksums.txt`; the annotated tag `vX.Y.Z`; R2 `mcp/vX.Y.Z/` (immutable,
   `version.json` written last) and `mcp/latest/` (never moved back); a GitHub draft with the nine assets; npm; the MCP
   Registry; then the GitHub release, published as Latest. Then every channel is checked from the outside: npm latest
   and integrity, a fresh `npm install` that downloads and runs the R2 binary, `npm audit signatures`, the registry
   entry, GitHub Latest and its asset digests, and R2 `vX.Y.Z/` and `latest/`.
5. **hosted** waits until `https://mcp.contextstream.io/health` serves the version. The private deployment notices the
   new GitHub Latest on its own (within about 5 minutes), builds and smoke-tests the gateway image and rolls it; this
   repository never dispatches a private deployment.

## Dry run

```
depot ci dispatch --repo contextstream/mcp-server --workflow release.yml --ref main --input mode=dry-run
```

It gates, builds and assembles the bundle, packs npm (`npm publish --dry-run`) and validates `server.json` with
`mcp-publisher`, and publishes nothing. Any run that is not a clean checkout of `main` (a branch, or a laptop
`depot ci run` with a patch) is a dry run, whatever mode it asks for.

## Re-running after a failure

Run it again; nothing needs cleaning up first:

```
depot ci dispatch --repo contextstream/mcp-server --workflow release.yml --ref main --input mode=release
```

Every step skips what is already done with the same bytes, and refuses different bytes:

- once the tag exists, its commit is the release, whatever `main` holds since;
- once R2 holds the complete bundle (`version.json` present), the run builds nothing and publishes those bytes;
- an npm version, a registry entry, GitHub assets and R2 objects that already exist must match, or the run stops.

A bad artifact that is already public is never replaced: fix forward with the next patch version.

## Secrets

Depot CI secrets, each with one variant limited to repository `contextstream/mcp-server`, branch `main` and workflow
`release.yml`. Only the publish step of a release run receives them; none is printed, written to a file or passed on a
command line.

| Secret | Used for |
|---|---|
| `MCP_RELEASE_GITHUB_TOKEN` | pushing the tag, the GitHub release and its assets, and the MCP Registry login (an org Owner's token: the registry grants `io.github.contextstream/*` only to Owners) |
| `MCP_RELEASE_NPM_TOKEN` | `npm publish` of `@contextstream/mcp-server` |
| `MCP_RELEASE_R2_BUCKET_URL` | the S3 endpoint and bucket of the release bucket |
| `MCP_RELEASE_R2_ACCESS_KEY_ID`, `MCP_RELEASE_R2_SECRET_ACCESS_KEY` | a key scoped to that bucket |

## When a darwin link fails

A dependency update that starts using a new macOS framework symbol fails the darwin build with
`undefined symbol: _Name`. Add the name to the matching stub in `scripts/release/macos-link-stubs` (see its README).
