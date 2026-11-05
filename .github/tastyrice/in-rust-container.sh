#!/bin/bash
# Runs a shell command in $RUST_IMAGE with the repo at /src, caches under ~/cargo-registry,
# ~/cargo-git and ~/target (restored/saved by actions/cache), and hands the files back to the
# runner user so the cache step can read them. DOCKER_EXTRA_ARGS adds docker run flags.
set -euo pipefail
mkdir -p ~/cargo-registry ~/cargo-git ~/target
# shellcheck disable=SC2086  # CARGO_ENV is a list of -e flags
exec docker run --rm $CARGO_ENV ${DOCKER_EXTRA_ARGS:-} -e CARGO_BUILD_JOBS="$(nproc)" \
  -v "$PWD:/src" -v "$HOME/cargo-registry:/usr/local/cargo/registry" \
  -v "$HOME/cargo-git:/usr/local/cargo/git" -v "$HOME/target:/target" \
  -w /src/codex-rs "$RUST_IMAGE" \
  bash -c "set -euo pipefail
    apt-get -qq update >/dev/null && apt-get -qq install -y libcap-dev pkg-config libssl-dev >/dev/null
    git config --global --add safe.directory '*'
    status=0; bash -euo pipefail -c \"\$1\" || status=\$?
    chown -R $(id -u):$(id -g) /target /usr/local/cargo/registry /usr/local/cargo/git
    exit \$status" _ "$1"
