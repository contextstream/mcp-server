# macOS link stubs

The release builds the two darwin binaries on Linux runners, because the release CI has no macOS runners.
`scripts/release/build.sh` makes a macOS SDK root without Apple's SDK and builds against it with stock LLVM
from Ubuntu: clang compiles the C dependencies (jemalloc, aws-lc, ring) and ld64.lld links. The SDK root holds:

- the macOS libc headers and the `libSystem` stub that [Zig](https://ziglang.org) ships (the build uses no
  other part of Zig);
- the stubs in this directory for the three system frameworks the server links and for `libiconv`;
- header shims in `include/` for what Zig's headers lack, added only where Zig has no such header.

The stubs (`.tbd`) are written for this repository; they are not copied from Apple's SDK. Each one names:

- the library's install name and versions, exactly as a binary built on a Mac records them (`otool -L`);
- the symbols the server imports from that library (`nm -m -u` on a binary built on a Mac).

At run time macOS loads the real system framework, as it does for a binary linked on a Mac.
`scripts/release/mcp_release.py macho-check` verifies every darwin build before it can be published: the exact
set of linked libraries, the minimum macOS version (11.0 on arm64, 10.12 on x64), the code signature on arm64
and the version string.

When a dependency update starts using a framework symbol that is not listed, the darwin link fails with
`undefined symbol: _Name`. Add the name to the stub of the framework that exports it, in sorted order, and
rebuild. A new framework needs a new stub here and an entry in `DARWIN_DYLIBS` in `mcp_release.py`.
