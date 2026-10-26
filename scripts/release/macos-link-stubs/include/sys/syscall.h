/* jemalloc (tikv-jemalloc-sys) includes <sys/syscall.h> on every Unix, and Zig's macOS headers do not ship it.
 * jemalloc makes no raw system call on macOS: its configure test for syscall(2) fails on a Mac, where syscall(2)
 * is deprecated and the test compiles with -Werror, and it fails here because SYS_write stays undefined. The
 * build adds this header to Zig's macOS headers only when Zig lacks it, so it shadows nothing. */
#ifndef CONTEXTSTREAM_RELEASE_SYS_SYSCALL_H
#define CONTEXTSTREAM_RELEASE_SYS_SYSCALL_H
#endif
