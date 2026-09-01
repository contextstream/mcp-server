<!-- mcp-name: io.github.contextstream/mcp-server -->

# ContextStream MCP Server

This repository is the canonical open-source implementation and release source
for the ContextStream Rust MCP server. It provides project memory, semantic code
search, dependency analysis, grounded context, and editor setup over the Model
Context Protocol.

The hosted service at `https://mcp.contextstream.io/mcp` is the recommended
transport. The npm package is a compatibility launcher for clients that require
stdio: it downloads the binary for the package's exact version from an immutable
release path, verifies SHA-256, and runs it from a versioned cache.

## Connect

Streamable HTTP (recommended):

```json
{
  "mcpServers": {
    "contextstream": {
      "type": "http",
      "url": "https://mcp.contextstream.io/mcp"
    }
  }
}
```

npm/stdin compatibility:

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server@1.0.0"],
      "env": {
        "CONTEXTSTREAM_API_KEY": "your-api-key"
      }
    }
  }
}
```

All 1.x releases preserve these npm executables:

- `mcp-server`
- `contextstream-mcp`
- `contextstream-hook` (forwards to the Rust `hook` subcommand)

Run the setup wizard with:

```bash
npx -y @contextstream/mcp-server@1.0.0 setup
```

## What setup sends

Setup prints this disclosure before changing editor configuration:

- Transcript exchange saving and hook transcript saving are enabled by default.
- A validated project can be indexed; matching source files are sent to the
  user's ContextStream workspace.
- Selected supported editors receive managed lifecycle hooks.
- Local Git capture is enabled by default. It sends event type, commit SHA/time,
  branch/ref names, aggregate line/file counts, a bounded redacted commit
  subject, an opaque checkout ID, and a credential-free canonical remote.
- Absolute filesystem paths, commit bodies, and commit author name/email are not
  sent by the VCS hook pipeline.

Controls and deletion paths are documented in
[`docs/data-handling.md`](docs/data-handling.md). These defaults are preserved
for compatibility; review them before running `setup --yes` in automation.

## Build

The workspace requires the Rust toolchain pinned in `rust-toolchain.toml`.

```bash
cargo build --locked -p mcp-server --bin contextstream-mcp
cargo test --locked --workspace
cargo build --locked --release -p mcp-server --bin contextstream-mcp \
  --features remote-acceleration
```

The public workspace contains seven crates:

- `mcp-server`
- `mcp-tools`
- `mcp-client`
- `mcp-session`
- `mcp-types`
- `mcp-model-registry`
- `mcp-acceleration-products`

`mcp-acceleration-products` is MongoDB-free. Premium provider implementation,
production deployment configuration, credentials, and private operator material
are not part of this repository. Compatibility traits remain no-op in public
builds so the core protocol surface stays stable.

## npm launcher behavior

The dependency-free Node 20 launcher:

- selects the package-version/platform artifact;
- downloads only from `mcp/v<exact-version>/`;
- verifies `version.json`, `checksums.txt`, and the binary SHA-256;
- populates the cache under an atomic lock and re-verifies cached bytes;
- supports `CONTEXTSTREAM_MCP_OFFLINE=1` after a verified first run;
- executes the cached binary by absolute path, ignoring PATH-shadowing copies;
- sets `CONTEXTSTREAM_DISABLE_SELF_UPDATE=1`, so the Rust process cannot mutate
  npm-managed cache state.

For test mirrors only, the launcher recognizes
`CONTEXTSTREAM_MCP_RELEASE_BASE_URL` together with
`CONTEXTSTREAM_MCP_TEST_ALLOW_HTTP=1`. Production release URLs must use HTTPS.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and
[GOVERNANCE.md](GOVERNANCE.md). By contributing, you certify the Developer
Certificate of Origin for your commits (`git commit -s`).

The code in this repository is MIT licensed. ContextStream trademarks and the
hosted backend are addressed in [NOTICE](NOTICE).
