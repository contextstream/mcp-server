# syntax=docker/dockerfile:1.7
# The machine the release gate runs on when the release runs on the self-hosted box (scripts/release/gate-on-box.sh): the
# root of scripts/dev/ns-runner.sh's instances (the same Ubuntu and apt packages, without the build-only docker, wine and fio,
# plus qemu-user and the arm64 C runtime that check the cross-built linux-arm64 binary;
# Node; user runner, uid 1001, with sudo) with the Rust toolchain rust-toolchain.toml names baked in (RUST_CHANNEL), with
# every release target's standard library, and owned by runner (build.sh's rustup target add runs as runner).
ARG UBUNTU=public.ecr.aws/docker/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
FROM ${UBUNTU}
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
