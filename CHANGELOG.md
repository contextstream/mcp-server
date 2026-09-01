# Changelog

## 1.0.0

- Replaced the legacy TypeScript implementation with the canonical Rust MCP
  server while preserving public repository history.
- Added the MongoDB-free remote acceleration build.
- Added a dependency-free npm compatibility launcher with exact-version
  downloads, SHA-256 verification, atomic caching, offline reuse, and the
  `mcp-server`, `contextstream-mcp`, and `contextstream-hook` aliases.
- Added dual Streamable HTTP and npm stdio MCP Registry metadata.
- Minimized VCS capture to opaque checkout IDs, credential-free remotes,
  bounded/redacted subjects, and aggregate metadata; author identity and raw
  paths are not transmitted.
- Moved build, security, attestation, and release authority to this repository.
