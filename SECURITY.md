# Security policy

## Supported versions

The latest stable 1.x release receives security fixes. Older 0.3/0.4 TypeScript
releases remain available for compatibility but are not the active development
line.

## Reporting a vulnerability

Please use GitHub private vulnerability reporting for this repository. Do not
open a public issue containing exploit details, credentials, private paths, or
customer data. If private reporting is unavailable, contact
`security@contextstream.io` with the repository, affected version, impact, and a
minimal reproduction.

We will acknowledge a report, validate scope, coordinate a fix and disclosure,
and credit reporters who want attribution. Do not test against accounts or data
you do not own.

## Supply chain

Release artifacts are tied to a public tag, checksummed, attested by GitHub, and
published first to an immutable versioned path. The npm package contains a small
dependency-free launcher and verifies the exact-version binary before execution.
Release CI runs secret scanning, dependency advisory checks, license policy,
CodeQL, and SBOM generation.
