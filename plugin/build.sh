#!/bin/sh
# Builds the component and puts it at the manifest's `entry`, wherever CARGO_TARGET_DIR points.
set -eu
cd "$(dirname "$0")"
cargo build --release --target wasm32-wasip2
out="${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/haunt_plugin.wasm"
if [ "$out" != "target/wasm32-wasip2/release/haunt_plugin.wasm" ]; then
  mkdir -p target/wasm32-wasip2/release
  cp "$out" target/wasm32-wasip2/release/haunt_plugin.wasm
fi
echo "built plugin/target/wasm32-wasip2/release/haunt_plugin.wasm; install with: haunt plugin install plugin"
