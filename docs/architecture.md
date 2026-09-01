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
