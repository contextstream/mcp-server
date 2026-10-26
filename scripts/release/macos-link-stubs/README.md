# macOS link stubs

The release builds the two darwin binaries on Linux runners with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) (Zig as the C
compiler and linker), because the release CI has no macOS runners.

Zig ships the stubs for `libSystem`. The binary also links three system
frameworks and `libiconv`, and the linker needs a text stub (`.tbd`) for each
to record the dependency. These stubs are written for this repository; they
are not copied from Apple's SDK. Each one names:

- the library's install name and versions, exactly as a binary built on a Mac
  records them (`otool -L`);
- the symbols the server imports from that library (`nm -m -u` on a binary
  built on a Mac).

At run time macOS loads the real system framework, as it does for a binary
linked on a Mac. `scripts/release/mcp_release.py macho-check` verifies every
darwin build before it can be published: the exact set of linked libraries,
the minimum macOS version, the code signature on arm64 and the version string.

When a dependency update starts using a framework symbol that is not listed,
the darwin link fails with `undefined symbol: _Name`. Add the name to the
stub of the framework that exports it, in sorted order, and rebuild. A new
framework needs a new stub here and an entry in `DARWIN_DYLIBS` in
`mcp_release.py`.
