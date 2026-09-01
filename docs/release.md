# Release process

Stable 1.x releases are produced only from a protected `v<version>` tag whose
commit has already passed public CI. `Cargo.toml`, `package.json`, `server.json`,
and the tag must contain the same canonical stable SemVer.

The release workflow builds the exact commit on hosted runners, tests the npm
launcher and registry metadata, scans source and dependencies, generates
checksums and SBOMs, and creates GitHub artifact attestations. Versioned R2
objects under `mcp/v<version>/` are immutable. The `mcp/latest/` alias, GitHub
release publication, npm `latest`, and MCP Registry publication occur only
after the immutable artifacts verify byte-for-byte.

## Required repository configuration

Protect `main` and the `v*` tag namespace. Require CI, CodeQL, DCO, review, and
conversation resolution on `main`; do not allow force pushes or tag deletion.
Release tags are annotated and must point to a commit already reachable from
the protected `main` branch.

For the initial Rust replacement, preserve the existing TypeScript line in this
same repository. Before merging the replacement commit, publish and protect
`legacy/typescript-v0.4` at the pre-replacement commit
`64d8e4e2760e9e17ede3eb691d4dda4d4d9c2aae`. Do not rewrite the repository,
delete existing tags/releases, or force-push `main`; the Rust tree lands as a
normal descendant commit.

Create a protected GitHub environment named `public-release` with required
maintainer reviewers and no self-approval. Configure these environment values:

- secret `PRIVACY_LEGAL_APPROVAL_EVIDENCE` — reference to the approval record;
- secret `INITIAL_RELEASE_KEY_ROTATION_EVIDENCE` — required specifically for
  1.0.0 and must reference completed rotation/revocation verification;
- secrets `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` — the token must be
  limited to writes for the release bucket only;
- variable `R2_BUCKET` — the public release bucket name.

The npm package must trust GitHub Actions publisher
`contextstream/mcp-server`, workflow `release.yml`, environment
`public-release`, for `npm publish`. The workflow uses npm OIDC on a hosted
runner and intentionally has no npm token. Once trusted publishing is tested,
disallow token publishing for the package and revoke the former automation
token. MCP Registry publication likewise uses GitHub OIDC.

The `public-release` environment and both evidence secrets are hard gates, not
places to paste approval prose created by the release run itself. Evidence must
exist before the annotated 1.0.0 tag is created.

## Release sequence

1. Confirm the candidate tree and the preserved public TypeScript history pass
   full-history and worktree secret scans.
2. Record Privacy/Legal approval and historical credential rotation/revocation
   evidence outside the repository.
3. For 1.0.0, publish the protected `legacy/typescript-v0.4` branch at the
   recorded TypeScript tip. Merge the Rust replacement through protected
   `main`, then create and push the annotated `v<version>` tag. Manual workflow
   dispatch is validation-only.
4. The workflow builds six binaries, generates an SBOM and checksums, and
   attests the exact payload.
5. After environment approval, it publishes immutable R2 bytes, a verified
   GitHub release, the OIDC-authenticated npm launcher, and dual remote/npm MCP
   Registry metadata. GitHub release finalization happens only after every
   channel succeeds.
6. The private deployment repository independently verifies every channel,
   opens an immutable pin PR, runs a canary, and requires its own protected
   production approval. The public repository never dispatches a private
   deployment.

Required external gates for the initial 1.0.0 replacement are documented in the
release environment: historical credential rotation/revocation and
Privacy/Legal approval. The workflow must not be used to bypass those approvals.

If promotion fails, do not overwrite a versioned object or retarget the tag.
An identical rerun may resume incomplete channels, but any byte difference is a
hard failure. Otherwise fix forward with a new version. Production consumers
keep the previously pinned digest for rollback.
