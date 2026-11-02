#!/bin/bash
# The remote half of scripts/dev/ns-run (scripts/dev/ns-run.py ships it on every run): mcp-server's copy of contextstream's
# scripts/dev/ns-runner.sh (contextstream main 5a7ad0ec1). Keep TREE, COMMIT and the modes in step with it. The root is the same:
# Rust (rustup in the cache's home, rust-toolchain.toml's channel), Node, sccache and docker for the gate and the builds;
# scripts/release/build.sh installs its own cross tools (clang, lld, wine, cargo-xwin, zig) with sudo.
# It runs as root on a Namespace instance (Wolfi host, docker, busybox tools; no python). The instance has a CACHE
# volume at /cache that holds, between runs:
#   rootfs/   an Ubuntu 24.04 root with the apt packages, node, sccache and the user runner (uid 1001); read-only in a run
#   home/     /home/runner in a run: rustup, cargo registry, npm cache, the cargo target dir (home/target, linked as
#             /work/target; incremental, as release.yml's rust part builds: a kept target, no sccache, line tables only)
#   tree/     a git repo whose HEAD is the merge base the last run used: that commit's own object, with no history
#             (.git/shallow), never with a patch. It counts only with its labels, .git/nsrun-base and .git/nsrun-tree,
#             which a run writes after putting it back exactly at its base and checking that (TREE below)
# Modes:
#   probe     print what the cache holds (tree base, rootfs version), one key=value per line
#   run       read the tar the client sent (meta, delta.diff or tree.tar.zst, patch.diff) from /tmp/nsrun/in and run
#             the command; exit with the command's status. The verdict's kind goes to /tmp/nsrun/status, which the
#             client reads: "cmd <rc> <MB used> <ok|bad>", "timeout <rc> <MB> <ok|bad>" (ok: the tree was put back
#             exactly at its base and labelled), "patch 3 <MB>" (the changes did not apply), "tree 4 <MB>" (no exact tree
#             of the base here: the client sends the whole tree, or gives up with no verdict) or "infra" (this runner
#             failed)
#   artifacts print the artifacts tar of the last run on stdout
#   finish keep|drop  tell the instance's main container to exit 0 (keep: the cache volume becomes the next runs'
#             starting point) or 1 (drop: it is abandoned); either ends the instance. ns-run.py says it directly.
#   tree-start, tree-finish  (the tests, with NSRUN_PLAIN_GIT=1, NSRUN_C and NSRUN_NS: plain git on $NSRUN_C/tree) the
#             tree steps of run alone: set up the exact tree and the patch, and put it back and label it.
set -uo pipefail
NS=${NSRUN_NS:-/tmp/nsrun}; IN=$NS/in; C=${NSRUN_C:-/cache}; ROOTFS_VERSION=nsrun-mcp-server-rootfs-v1-$(uname -m)
UBUNTU=public.ecr.aws/docker/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
NODE_VERSION=22.23.2
SCCACHE_VERSION=v0.18.0
# ARM64: the release's linux-arm64 binary builds on an arm64 instance (scripts/dev/ns-run --arch arm64). The Ubuntu pin
# above is a multi-arch index, so docker takes its arm64 image there; Node, sccache and nsc come in the host's own arch.
case "$(uname -m)" in
  aarch64|arm64) ARCH=arm64; NODE_ARCH=arm64; SCCACHE_TRIPLE=aarch64-unknown-linux-musl
                 SCCACHE_SHA256=2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a
                 NSC_SHA256=275d5bbfb64f108e87dd179d962abfbf41284d9013a2429dd9d8c0e40bd00b20 ;;
  *)             ARCH=amd64; NODE_ARCH=x64; SCCACHE_TRIPLE=x86_64-unknown-linux-musl
                 SCCACHE_SHA256=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89
                 NSC_SHA256=ba46d10dcb31caff49eda5597c6208e8917c490a0602480d6de127688f287a10 ;;
esac
APT_PACKAGES="build-essential pkg-config libssl-dev libpq-dev cmake clang mold protobuf-compiler zstd git curl ca-certificates python3 python3-yaml jq bc sudo xz-utils util-linux openssh-client docker.io fio"
FINISH_PORT=47123
# nsc, for the compile cache's credentials (pinned; kept on the volume as bin/nsc-<version>)
NSC_VERSION=0.0.588
NSC_URL=https://get.namespace.so/packages/nsc/v$NSC_VERSION/nsc_${NSC_VERSION}_linux_$ARCH.tar.gz
note() { echo "nsrun: $*" >&2; }
el() { echo $(( $(date +%s) - $1 )); }
used_mb() { df -Pk $C 2>/dev/null | awk 'NR==2 {printf "%d", $3 / 1024}'; }
# A git command on the tree as runner (uid 1001), inside the Ubuntu root (the host's busybox cannot switch user):
# the tree at /work, this run's inputs at /mnt; a private mount namespace, gone when the command exits.
tree_git() {
  if [ "${NSRUN_PLAIN_GIT:-}" = 1 ]; then   # the tests: plain git on $C/tree, as this user
    env -i HOME=/tmp PATH=/usr/bin:/bin GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=nsrun GIT_AUTHOR_EMAIL=nsrun@localhost \
      GIT_COMMITTER_NAME=nsrun GIT_COMMITTER_EMAIL=nsrun@localhost ${GIT_INDEX_FILE:+GIT_INDEX_FILE=$GIT_INDEX_FILE} \
      git -c apply.whitespace=nowarn -C $C/tree "$@"
    return
  fi
  unshare -m bash -c '
    R=/tmp/nsrun/gitroot; mkdir -p $R
    mount --bind /cache/rootfs $R && mount -o remount,bind,ro $R && mount -t proc proc $R/proc && mount --rbind /dev $R/dev && mount -t tmpfs tmpfs $R/tmp \
      && mount --bind /cache/tree $R/work && mount --bind /tmp/nsrun/in $R/mnt || exit 75
    exec chroot $R /usr/bin/setpriv --reuid=1001 --regid=1001 --init-groups /usr/bin/env -i HOME=/tmp PATH=/usr/bin:/bin \
      GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=nsrun GIT_AUTHOR_EMAIL=nsrun@localhost GIT_COMMITTER_NAME=nsrun \
      GIT_COMMITTER_EMAIL=nsrun@localhost ${GIT_INDEX_FILE:+GIT_INDEX_FILE=$GIT_INDEX_FILE} git -c apply.whitespace=nowarn \
      -C /work "$@"' bash "$@"
}
# Where this run's inputs and the tree's git dir are, as tree_git sees them.
in_dir() { if [ "${NSRUN_PLAIN_GIT:-}" = 1 ]; then echo "$IN"; else echo /mnt; fi; }
git_dir() { if [ "${NSRUN_PLAIN_GIT:-}" = 1 ]; then echo "$C/tree/.git"; else echo /work/.git; fi; }

probe() {
  local base tree
  base=$(cat $C/tree/.git/nsrun-base 2>/dev/null); tree=$(cat $C/tree/.git/nsrun-tree 2>/dev/null)
  # TREE: a tree without both labels (an older runner's, or one a run left half-way) is no tree at all.
  { [ -n "$base" ] && [ -n "$tree" ]; } || base=none
  echo "tree_base=$base"
  echo "rootfs=$(cat $C/rootfs/.nsrun-version 2>/dev/null || echo none)"
  echo "cache_used=$(used_mb)MB"
}

# The cache volume is 120 GB; the cargo target dir is what grows. Past 85 % full it goes, and the next build is cold.
bound_cache() {
  rm -rf $C/home/.cache/sccache   # left by nsrun-rootfs-v1 runs, which compiled through sccache
  local size used
  read -r size used <<< "$(df -Pk $C 2>/dev/null | awk 'NR==2 {print $2, $3}')"
  [ -n "${size:-}" ] && [ "$size" -gt 0 ] || return 0
  if [ $((used * 100 / size)) -ge 85 ]; then
    note "cache $((used * 100 / size))% full: removing the cargo target dir"
    rm -rf $C/home/target
  fi
}

# COMPILE CACHE: sccache on Namespace's HTTP cache (WebDAV). The client named one cache ($IN/sccache_cache) and gave the
# instance's workload identity cache/httpcache on it (ensure, read, write); `nsc cache sccache setup` here, as root on the
# host, turns that identity into a short-lived endpoint and token for the run. No credential comes from outside. Any
# failure means building without it.
sccache_setup() {
  local name site nsc=$C/bin/nsc-$NSC_VERSION t=$NS/nsc.tgz
  name=$(cat $IN/sccache_cache 2>/dev/null); site=$(cat $IN/sccache_site 2>/dev/null)
  rm -f $NS/sccache.env
  [ -n "$name" ] || return 0
  if [ ! -x $nsc ]; then
    { curl -fsSL --retry 3 -o $t "$NSC_URL" && echo "$NSC_SHA256  $t" | sha256sum -c - >/dev/null 2>&1 \
      && rm -rf $NS/nsc && mkdir -p $NS/nsc $C/bin && tar -xzf $t -C $NS/nsc nsc && chmod 755 $NS/nsc/nsc && mv $NS/nsc/nsc $nsc; } \
      || { note "compile cache: could not fetch nsc $NSC_VERSION; building without it"; rm -f $t; return 0; }
    rm -f $t
  fi
  ( umask 077; $nsc cache sccache setup --cache_name "$name" ${site:+--site "$site"} > $NS/sccache.env 2> $NS/sccache.err )
  if grep -q '^SCCACHE_WEBDAV_ENDPOINT=' $NS/sccache.env 2>/dev/null && grep -q '^SCCACHE_WEBDAV_TOKEN=' $NS/sccache.env; then
    note "compile cache: sccache on $(sed -n 's/^SCCACHE_WEBDAV_ENDPOINT=//p' $NS/sccache.env) ($name)"
  else
    note "compile cache: unavailable ($(grep -v -i token $NS/sccache.err 2>/dev/null | grep -v '^$' | head -1 | cut -c1-120)); building without it"
    rm -f $NS/sccache.env
  fi
  rm -f $NS/sccache.err
}

ensure_rootfs() {
  [ "$(cat $C/rootfs/.nsrun-version 2>/dev/null)" = "$ROOTFS_VERSION" ] && { note "rootfs: cached ($ROOTFS_VERSION)"; return 0; }
  local t=$(date +%s)
  note "rootfs: building $ROOTFS_VERSION (cache miss)"
  docker rm -f nsrun-rootfs >/dev/null 2>&1
  docker run --name nsrun-rootfs "$UBUNTU" bash -c "
    set -e; export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq --no-install-recommends $APT_PACKAGES >/dev/null
    curl -fsSL https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-linux-$NODE_ARCH.tar.xz | tar -xJ -C /usr/local --strip-components=1
    curl -fsSL -o /tmp/s.tgz https://github.com/mozilla/sccache/releases/download/$SCCACHE_VERSION/sccache-$SCCACHE_VERSION-$SCCACHE_TRIPLE.tar.gz
    echo '$SCCACHE_SHA256  /tmp/s.tgz' | sha256sum -c --quiet -
    tar -xzf /tmp/s.tgz -C /tmp && install -m 0755 /tmp/sccache-$SCCACHE_VERSION-$SCCACHE_TRIPLE/sccache /usr/local/bin/sccache
    userdel -r ubuntu >/dev/null 2>&1 || true
    useradd -m -u 1001 -s /bin/bash runner && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner && chmod 440 /etc/sudoers.d/runner
    groupadd -f docker && usermod -aG docker runner
    mkdir -p /work /data && apt-get clean && rm -rf /var/lib/apt/lists/*" >&2 || { note "rootfs build failed"; return 1; }
  rm -rf $C/rootfs.new && mkdir -p $C/rootfs.new
  # GNU tar from the image itself unpacks the export with owners and modes intact.
  docker export nsrun-rootfs | docker run --rm -i -v $C:/c "$UBUNTU" tar -x -f - -C /c/rootfs.new --numeric-owner || return 1
  docker rm -f nsrun-rootfs >/dev/null 2>&1
  echo "$ROOTFS_VERSION" > $C/rootfs.new/.nsrun-version
  rm -rf $C/rootfs && mv $C/rootfs.new $C/rootfs
  note "rootfs: built in $(el $t)s"
}

# TREE (2026-11-01: run nkf3i3lkun5r6 found a tree labelled cd391b849 whose files weren't cd391b849's, and ran on a
# stand-in HEAD). The tree counts only with its labels, nsrun-base and nsrun-tree, written by finish_tree after it put the
# tree back exactly at the base and checked that. A run removes them first, so a tree a run left half-way (a destroyed
# instance keeps its volume as it stands) has none and isn't used. Every run checks the tree it gets: HEAD is the base
# commit itself, with that commit's tree, nothing else is in the tree, and base plus the patch is the tree the client
# computed. When any of that fails, the run reports "tree 4" and runs nothing; the client sends the whole tree, once.
write_labels() { echo "$1" > $C/tree/.git/nsrun-base && echo "$2" > $C/tree/.git/nsrun-tree; }
drop_labels() { rm -f $C/tree/.git/nsrun-base $C/tree/.git/nsrun-tree; }
drop_tree() { rm -rf $C/tree; }
tree_ok() { # base want: HEAD is base itself, its tree and the index are want, and the tree holds nothing else
  [ "$(tree_git rev-parse -q --verify HEAD 2>/dev/null)" = "$1" ] \
    && [ "$(tree_git rev-parse -q --verify 'HEAD^{tree}' 2>/dev/null)" = "$2" ] \
    && [ "$(tree_git write-tree 2>/dev/null)" = "$2" ] && tree_clean
}
tree_clean() { local st; st=$(tree_git status --porcelain --untracked-files=all 2>/dev/null) && [ -z "$st" ]; }

# HEAD becomes the base commit itself: its own object (the client sends `git cat-file commit <base>`), with no history
# (.git/shallow), so `git rev-parse HEAD` is the base and `git status` is clean, as in a checkout of it; validate.py
# --release checks both. Only when the index holds exactly the base's tree; never a stand-in.
set_head() {
  local base=$1 want=$2 got
  got=$(tree_git write-tree) || return 1
  if [ -z "$want" ] || [ "$got" != "$want" ]; then
    note "tree: not ${base:0:12}'s tree (got ${got:0:12}, want ${want:0:12})"; return 1
  fi
  [ "$(tree_git hash-object -t commit -w --stdin < $IN/base.commit)" = "$base" ] \
    || { note "tree: ${base:0:12}'s own commit could not be rebuilt"; return 1; }
  echo "$base" > $C/tree/.git/shallow && { [ "${NSRUN_PLAIN_GIT:-}" = 1 ] || chown 1001:1001 $C/tree/.git/shallow; } \
    && tree_git update-ref HEAD "$base"
}

ensure_tree() { # base want: the exact tree of base, from delta.diff (warm) or tree.tar.zst (cold); 4 when not
  local base=$1 want=$2 t=$(date +%s) from from_tree
  if [ -f $IN/delta.diff ]; then
    from=$(cat $IN/from 2>/dev/null); from_tree=$(cat $C/tree/.git/nsrun-tree 2>/dev/null)
    if [ -z "$from" ] || [ "$(cat $C/tree/.git/nsrun-base 2>/dev/null)" != "$from" ] || [ -z "$from_tree" ]; then
      note "tree: the cache doesn't hold ${from:0:12} any more"; return 4
    fi
    drop_labels
    if ! { tree_git reset -q --hard "$from" && tree_git clean -fdxq && tree_ok "$from" "$from_tree"; }; then
      note "tree: the cached tree labelled ${from:0:12} isn't ${from:0:12}'s; it goes"; drop_tree; return 4
    fi
    if ! { { [ ! -s $IN/delta.diff ] || tree_git apply --binary --index "$(in_dir)/delta.diff"; } && set_head "$base" "$want"; }; then
      note "tree: the delta from ${from:0:12} did not give ${base:0:12}'s tree; the tree goes"; drop_tree; return 4
    fi
    note "tree: ${from:0:12} -> ${base:0:12} by delta ($(wc -c < $IN/delta.diff) bytes) in $(el $t)s"
  elif [ -f $IN/tree.tar.zst ]; then
    drop_tree; mkdir -p $C/tree && zstd -dc $IN/tree.tar.zst | tar -x -C $C/tree || return 1
    [ "${NSRUN_PLAIN_GIT:-}" = 1 ] || chown -R 1001:1001 $C/tree || return 1
    if ! { tree_git init -q && tree_git add -A -f && set_head "$base" "$want"; }; then
      note "tree: the whole tree sent for ${base:0:12} isn't its tree"; drop_tree; return 4
    fi
    note "tree: ${base:0:12} from a full tree ($(wc -c < $IN/tree.tar.zst) bytes) in $(el $t)s"
  else
    note "tree: neither a usable delta nor a full tree came"; return 4
  fi
  # The target dir is a link (inner.sh); /.gitignore's /target/ matches only directories.
  grep -qx /target $C/tree/.git/info/exclude 2>/dev/null || { mkdir -p $C/tree/.git/info && echo /target >> $C/tree/.git/info/exclude; }
  [ "${NSRUN_PLAIN_GIT:-}" = 1 ] || chown -R 1001:1001 $C/tree/.git/info
  tree_ok "$base" "$want" || { note "tree: ${base:0:12} did not come out clean; the tree goes"; drop_tree; return 4; }
}

changes_tree() { # the tree of HEAD plus the patch, made in a scratch index
  local idx
  idx="$(git_dir)/nsrun-check.idx"; rm -f $C/tree/.git/nsrun-check.idx
  GIT_INDEX_FILE=$idx tree_git read-tree HEAD \
    && { [ ! -s $IN/patch.diff ] || GIT_INDEX_FILE=$idx tree_git apply --cached --binary "$(in_dir)/patch.diff"; } \
    && GIT_INDEX_FILE=$idx tree_git write-tree
  rm -f $C/tree/.git/nsrun-check.idx
}

# The tree steps of a run: the exact tree of the base, the patch on it, and the check. Exit status 0, 3 (the patch did
# not apply), 4 (no exact tree: no verdict) or 1 (failed); the status file says the same.
tree_start() {
  local base=$1 want=$2 r head htree got expect
  ensure_tree "$base" "$want"; r=$?
  [ $r -ne 4 ] || { echo "tree 4 $(used_mb)" > $NS/status; return 4; }
  [ $r -eq 0 ] || return 1
  drop_labels   # in use: a run that ends half-way leaves the tree without labels
  if [ -s $IN/patch.diff ] && ! tree_git apply --binary "$(in_dir)/patch.diff"; then
    note "your changes did not apply to ${base:0:12}"
    ! tree_ok "$base" "$want" || write_labels "$base" "$want"   # a failed git apply changes nothing
    echo "patch 3 $(used_mb)" > $NS/status; return 3
  fi
  head=$(tree_git rev-parse -q --verify HEAD); htree=$(tree_git rev-parse -q --verify 'HEAD^{tree}')
  got=$(changes_tree); expect=$(cat $IN/expect_tree 2>/dev/null)
  note "tree: HEAD ${head:0:12}, its tree ${htree:0:12} (want ${want:0:12}); with your changes ${got:0:12} (want ${expect:0:12})"
  if [ "$head" != "$base" ] || [ "$htree" != "$want" ] || [ -z "$expect" ] || [ "$got" != "$expect" ]; then
    note "tree: not exactly ${base:0:12} plus your changes; nothing runs on it"
    drop_tree; echo "tree 4 $(used_mb)" > $NS/status; return 4
  fi
}

finish_tree() { # base want: back exactly at the base (the patch and all the command wrote go); labelled only then
  tree_git reset -q --hard "$1" >/dev/null 2>&1; tree_git clean -fdxq >/dev/null 2>&1
  tree_git update-ref -d refs/stash >/dev/null 2>&1
  if tree_ok "$1" "$2"; then
    write_labels "$1" "$2"; echo ok
  else
    note "tree: not put back exactly at ${1:0:12}; it keeps no labels, and the next run sends the whole tree"; echo bad
  fi
}

run() {
  local t0=$(date +%s) base want rc r kind state
  base=$(cat $IN/base); want=$(sed -n '1s/^tree //p' $IN/base.commit 2>/dev/null)
  echo infra > $NS/status
  mkdir -p $C/home && chown 1001:1001 $C/home
  ensure_rootfs || exit 75
  tree_start "$base" "$want"; r=$?
  [ $r -eq 0 ] || exit $([ $r -eq 1 ] && echo 75 || echo $r)
  bound_cache
  sccache_setup
  chmod 666 /var/run/docker.sock 2>/dev/null
  mkdir -p $NS/data && chown 1001:1001 $NS/data
  # --input files go under /data (the run's scratch), as the client listed them: "<n> /data/<path>".
  if [ -s $IN/inputs.list ]; then
    while read -r i dest; do
      rel=${dest#/data/}
      mkdir -p "$NS/data/$(dirname "$rel")" && cp "$IN/inputs/$i" "$NS/data/$rel" || exit 75
    done < $IN/inputs.list
    chown -R 1001:1001 $NS/data
  fi
  # What runs inside the root, as runner: toolchains (kept in the cache's home), then the command, unquoted by eval.
  cat > $NS/inner.sh <<'INNER'
#!/bin/bash
cd /work || exit 75
# The cargo target dir lives in the cache's home and is linked into the tree (no CARGO_TARGET_DIR: tests such as
# scripts/release/test-validate.py read it, as on a Depot runner where it is unset).
mkdir -p "$HOME/target" && { [ -e target ] || [ -L target ] || ln -s "$HOME/target" target; } || exit 75
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none >/dev/null 2>&1 || exit 75
fi
tc=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml 2>/dev/null | head -1)
if [ -n "$tc" ]; then
  rustup toolchain install >/dev/null 2>&1 || rustup toolchain install "$tc" >/dev/null 2>&1
  rustup default "$tc" >/dev/null 2>&1
fi
# The compile cache, when sccache_setup got one: only its SCCACHE_WEBDAV_* lines are taken.
if [ -f /run/nsrun/sccache.env ]; then
  while IFS= read -r line; do case "$line" in SCCACHE_WEBDAV_*=*) export "$line" ;; esac; done < /run/nsrun/sccache.env
  export RUSTC_WRAPPER=sccache SCCACHE_IGNORE_SERVER_IO_ERROR=1 SCCACHE_IDLE_TIMEOUT=0
  # sccache refuses to run at all under CARGO_INCREMENTAL=1. Without it, the workspace's profiles (incremental = true in
  # Cargo.toml's dev and test) keep workspace crates incremental, which sccache passes through, and the dependencies,
  # never incremental, come from the cache.
  unset CARGO_INCREMENTAL
  sccache --start-server >/dev/null 2>&1 || unset RUSTC_WRAPPER
fi
( eval "$(cat /run/nsrun/cmd)" )
rc=$?
if [ -n "${RUSTC_WRAPPER:-}" ]; then
  sccache --show-stats 2>/dev/null | grep -E '^(Compile requests|Cache hits|Cache misses|Non-cacheable calls) ' \
    | sed 's/   */ /g; s/^/nsrun: sccache: /' >&2
  sccache --stop-server >/dev/null 2>&1
fi
# The artifacts, resolved as the command saw them (target/ is a link into home): one path or glob per line.
if [ -s /run/nsrun/artifacts ]; then
  (
    IFS=$'\n'; files=()
    while read -r p; do [ -n "$p" ] || continue; for f in $p; do [ -e "$f" ] && files+=("$f"); done; done < /run/nsrun/artifacts
    [ ${#files[@]} -eq 0 ] || tar -chf /data/.nsrun-artifacts.tar "${files[@]}"
  ) 2>/dev/null
fi
exit $rc
INNER
  note "setup $(el $t0)s; running: $(cat $IN/cmd)"
  # A private mount namespace: the rootfs read-only, home/tree/data writable, /tmp fresh; everything unmounts at exit.
  local t=$(date +%s)
  timeout -k 30 "$(cat $IN/timeout_s)" unshare -m bash -c '
    set -e; R=/tmp/nsrun/root; mkdir -p $R
    mount --bind /cache/rootfs $R; mount -o remount,bind,ro $R
    mount -t proc proc $R/proc; mount --rbind /sys $R/sys; mount --rbind /dev $R/dev
    mount -t tmpfs -o size=50% tmpfs $R/tmp; mount -t tmpfs tmpfs $R/run
    mount --bind /cache/home $R/home/runner; mount --bind /cache/tree $R/work; mount --bind /tmp/nsrun/data $R/data
    mkdir -p $R/run/nsrun; cp /tmp/nsrun/inner.sh /tmp/nsrun/in/cmd /tmp/nsrun/in/artifacts $R/run/nsrun/; chmod -R a+rX $R/run/nsrun
    [ ! -f /tmp/nsrun/sccache.env ] || { cp /tmp/nsrun/sccache.env $R/run/nsrun/sccache.env; chown 1001:1001 $R/run/nsrun/sccache.env; chmod 600 $R/run/nsrun/sccache.env; }
    touch $R/run/docker.sock; mount --bind /var/run/docker.sock $R/run/docker.sock
    mount --bind /etc/resolv.conf $R/etc/resolv.conf
    exec chroot $R /usr/bin/setpriv --reuid=1001 --regid=1001 --init-groups /usr/bin/env -i \
      HOME=/home/runner USER=runner LOGNAME=runner SHELL=/bin/bash LANG=C.UTF-8 TERM=dumb \
      PATH=/home/runner/.cargo/bin:/usr/local/bin:/usr/bin:/bin DOCKER_HOST=unix:///run/docker.sock \
      CARGO_INCREMENTAL=1 \
      CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only \
      CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=off CARGO_PROFILE_TEST_SPLIT_DEBUGINFO=off \
      CARGO_TERM_COLOR=never RUST_BACKTRACE=1 RUST_MIN_STACK=16777216 P2R4_WIRE_DIR=/data/dev/p2r4-wire \
      RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=mold" NSRUN=1 CI=1 \
      /bin/bash /run/nsrun/inner.sh
  '
  rc=$?
  kind=cmd
  if [ $(el $t) -ge "$(cat $IN/timeout_s)" ]; then
    note "the command timed out after $(cat $IN/timeout_s)s"; kind=timeout
  fi
  note "command exit $rc after $(el $t)s"
  rm -f $NS/artifacts.tar; [ ! -f $NS/data/.nsrun-artifacts.tar ] || mv $NS/data/.nsrun-artifacts.tar $NS/artifacts.tar
  # Leave the tree exactly at the base for the next run: the patch and everything the command wrote go (TREE).
  state=$(finish_tree "$base" "$want")
  echo "$kind $rc $(used_mb) $state" > $NS/status
  # ORPHAN: the client says keep or drop within seconds of this. A client that is gone (its machine was preempted) never
  # will, so 10 minutes from now the instance drops its cache volume and ends, instead of waiting for its deadline.
  ( trap '' HUP; sleep ${NSRUN_ORPHAN_S:-600}; echo drop > /dev/tcp/127.0.0.1/$FINISH_PORT ) </dev/null >/dev/null 2>&1 &
  exit $rc
}

case "${1:-}" in
  probe) probe ;;
  run) run ;;
  artifacts) cat $NS/artifacts.tar 2>/dev/null ;;
  status) cat $NS/status 2>/dev/null || echo infra ;;
  finish) [ "${2:-}" = keep ] && w=keep || w=drop; echo $w > /dev/tcp/127.0.0.1/$FINISH_PORT ;;
  tree-start) [ "${NSRUN_PLAIN_GIT:-}" = 1 ] || exit 2; mkdir -p $NS; tree_start "$(cat $IN/base)" "$(sed -n '1s/^tree //p' $IN/base.commit)" ;;
  tree-finish) [ "${NSRUN_PLAIN_GIT:-}" = 1 ] || exit 2; finish_tree "$(cat $IN/base)" "$(sed -n '1s/^tree //p' $IN/base.commit)" ;;
  *) echo "usage: ns-runner.sh probe|run|artifacts|status|finish" >&2; exit 2 ;;
esac
