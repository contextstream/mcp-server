# syntax=docker/dockerfile:1.7
# The machine the release gate can run in: Ubuntu 24.04 with the apt packages the gate needs, plus qemu-user and the arm64 C
# runtime that check the cross-built linux-arm64 binary; Node; user runner, uid 1001, with sudo; with the Rust toolchain
# rust-toolchain.toml names baked in (RUST_CHANNEL), with every release target's standard library, and owned by runner
# (build.sh's rustup target add runs as runner).
ARG UBUNTU=public.ecr.aws/docker/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
FROM ${UBUNTU} AS env
ARG NODE_VERSION=22.23.2
ARG RUST_CHANNEL
ENV DEBIAN_FRONTEND=noninteractive
RUN test -n "$RUST_CHANNEL" \
 && apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends build-essential pkg-config libssl-dev libpq-dev cmake clang lld llvm mold \
      protobuf-compiler zstd git curl ca-certificates python3 python3-yaml jq bc sudo xz-utils util-linux openssh-client \
      qemu-user libc6-arm64-cross >/dev/null \
 && curl -fsSL --retry 3 -o /tmp/node.tar.xz https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-linux-x64.tar.xz \
 && curl -fsSL --retry 3 https://nodejs.org/dist/v$NODE_VERSION/SHASUMS256.txt | grep " node-v$NODE_VERSION-linux-x64.tar.xz\$" \
      | sed "s# node-v.*# /tmp/node.tar.xz#" | sha256sum --check --strict --quiet \
 && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 && rm -f /tmp/node.tar.xz \
 && curl -fsSL --retry 3 https://sh.rustup.rs | env RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
      sh -s -- -y --profile minimal --default-toolchain "$RUST_CHANNEL" -c rustfmt -c clippy -c rust-src --no-modify-path \
      -t x86_64-unknown-linux-gnu -t aarch64-unknown-linux-gnu -t x86_64-pc-windows-msvc -t x86_64-apple-darwin -t aarch64-apple-darwin \
      >/dev/null \
 && useradd -m -u 1001 -s /bin/bash runner && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner \
 && chown -R runner:runner /usr/local/rustup /usr/local/cargo \
 && chmod 440 /etc/sudoers.d/runner && apt-get clean && rm -rf /var/lib/apt/lists/*
ENV PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin RUSTUP_HOME=/usr/local/rustup
USER runner
WORKDIR /home/runner

# The checkout, owned by runner; cargo's home is a cache mount in the stages below.
FROM env AS src
ENV CARGO_HOME=/home/runner/.cargo CI=true
WORKDIR /home/runner/src
COPY --chown=1001:1001 . .
RUN git config --global --add safe.directory '*'

# One gate part (scripts/release/gate.sh rust|checks): the build's exit status is the part's. RUN_ID differs per run, so a
# part always runs (cargo audit's advisories change while the tree does not).
FROM src AS gate
ARG PART
ARG RUN_ID
RUN --mount=type=cache,id=mcp-cargo-home,target=/home/runner/.cargo,uid=1001,gid=1001 \
    --mount=type=cache,id=mcp-gate-target-${PART},target=/home/runner/src/target,uid=1001,gid=1001,sharing=locked \
    echo "gate: run $RUN_ID, part $PART" && bash scripts/release/gate.sh "$PART"

# Windows' check runs the binary under Wine, and nasm assembles for its crypto.
FROM env AS build-env
USER root
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends nasm wine64 >/dev/null \
 && apt-get clean && rm -rf /var/lib/apt/lists/*
USER runner
# runner's own: a cache mount below it would otherwise make ~/.cache root's, where build.sh keeps its tools.
RUN mkdir -p /home/runner/.cache/cargo-xwin

# The checkout as the binaries build from it: every file of the commit, without .git. Two checkouts of one commit differ
# only under .git (the index holds each file's time, the logs the time of the fetch), and nothing build.sh runs reads it.
# BuildKit keys a COPY by what the files hold, not by their times, so a COPY from this stage is the same layer for the same
# tree, whichever run and checkout it comes from.
FROM env AS tree
COPY --chown=1001:1001 . /home/runner/tree
RUN rm -rf /home/runner/tree/.git

# One release binary (scripts/release/build.sh <artifact> <version>), exported by the stage binary as /<artifact>.
# Every cache mount a binary locks is its own (its id names the artifact), so the six binaries of a release build side by
# side on one builder. Until 1.0.72 cargo-xwin's mount was one for all six, and its lock made each build wait for the one
# before it: the six ended one after another, each about 5 to 6 minutes after the last.
#
# A release reuses the build of its dry run. The sources come from the stage tree, so the build step of a commit that was
# built before is a cache hit while the builder keeps that layer: same tree, same Dockerfile, RUST_CHANNEL, ARTIFACT, VERSION
# and SOURCE_DATE_EPOCH, and the release exports, byte for byte, the binary the dry run built and checked. A difference in any
# of them is a miss and an ordinary build. Until 1.0.77 the stage copied the context, .git included: no run ever hit, and
# the release compiled for 12 minutes what its dry run had compiled 13 minutes before.
#
# The step stamps the sources with its own time before cargo runs. A reused layer of the tree carries the file times of the
# run that made it, and cargo calls a crate fresh when no source file is newer than its last build in target/, which may
# since have built another tree. Stamped, a step that runs compiles the workspace crates, as it did from every new checkout.
FROM build-env AS build
ENV CARGO_HOME=/home/runner/.cargo CI=true
WORKDIR /home/runner/src
COPY --from=tree --chown=1001:1001 /home/runner/tree/ .
ARG ARTIFACT
ARG VERSION
ARG SOURCE_DATE_EPOCH
RUN --mount=type=cache,id=mcp-cargo-home,target=/home/runner/.cargo,uid=1001,gid=1001 \
    --mount=type=cache,id=mcp-build-target-${ARTIFACT},target=/home/runner/src/target,uid=1001,gid=1001,sharing=locked \
    --mount=type=cache,id=mcp-build-xwin-${ARTIFACT},target=/home/runner/.cache/cargo-xwin,uid=1001,gid=1001,sharing=locked \
    find . -path ./target -prune -o -type f -exec touch {} + \
 && git config --global --add safe.directory '*' && bash scripts/release/build.sh "$ARTIFACT" "$VERSION"

FROM scratch AS binary
COPY --from=build /home/runner/src/out/ /
