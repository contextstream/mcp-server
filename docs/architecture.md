# Public architecture and boundary

The repository contains the local stdio binary, HTTP transport, MCP domain
tools, shared types, session/checkout identity logic, model registry, setup and
installer code, and the MongoDB-free acceleration adapters used by the remote
gateway build.

The server is a client of ContextStream's hosted API. This repository does not
contain the hosted backend, production deployment overlay, credentials, private
operator runbooks, evaluation data, or premium provider implementation.

The `mcp-types` compatibility provider traits remain public because tool and
protocol types depend on them. `mcp-server` always constructs the no-op
compatibility layer. The optional `remote-acceleration` feature depends only on
`mcp-acceleration-products`; CI asserts that the dependency graph contains no
`mongodb` or `bson` packages.

Public GitHub releases, R2 objects, npm packages, and MCP Registry metadata are
all produced from the same protected public tag. Private production automation
may consume a verified public artifact digest, but it is not a publication
source.

## Remote checks and verification

The local client reads the hosted tool catalog, captures the checkout into a
temporary Git object store and opens the worker stream with a call-scoped
token. The CLI and the MCP `check` tool share this stream protocol. Editor
hooks can start background `:prepare` and opted-in speculative checks through
the same path. The hosted MCP gateway cannot read the editor's checkout.

The local `verify` tool asks the hosted planner for graph-selected and
verification-map checks. It can execute planned commands through `check` or
fall back to local execution. After an eligible MCP `check`, the client can
request consumer contract checks. The API resolves the consumers and supplies
the worker requests. Those streams carry the mapped contract changes. The
consumer's base tree must already be available on the worker.

The hosted API verifies signed usage reports and stores eligible passing
receipts. Local editor hooks query policy and receipts before supported actions.
Git's managed `pre-push` hook performs the push lookup synchronously, so Git
can stop before sending refs when the policy requires a receipt. Capsules and
handoffs can summarize stored verification results. [Data sent by each flow](data-handling.md#remote-checks-and-verification).
