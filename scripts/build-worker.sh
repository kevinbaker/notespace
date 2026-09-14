#!/bin/sh
# Builds the Worker wasm. Wrangler's [build] command runs this, so it has to work on a developer
# machine, on a CI runner, and in Cloudflare's Workers Builds image, which ships Node, Go, Python
# and Ruby but no Rust at all.
set -eu

WORKER_BUILD_VERSION="${WORKER_BUILD_VERSION:-^0.8}"
# /login, /register and /t/{id}/reply are compiled out without this, and 404.
WORKER_FEATURES="${WORKER_FEATURES:-password}"

if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain stable --target wasm32-unknown-unknown
fi

# /bin/sh does not read a shell profile, so ~/.cargo/bin is off PATH even once rustup has
# installed into it -- and a distro cargo in /usr/bin leaves no ~/.cargo/env to source.
if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
fi
PATH="$HOME/.cargo/bin:$PATH"
export PATH

# A no-op when a matching version is already installed.
cargo install -q "worker-build@${WORKER_BUILD_VERSION}"

cd "$(dirname "$0")/../crates/worker"
exec worker-build --release -- --features "$WORKER_FEATURES"
