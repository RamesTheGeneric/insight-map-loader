#!/usr/bin/env bash
# Launch the insight-prime GUI. Always runs from the repo root, because every path the
# service uses is relative to it (insight-prime.json, map/, transforms.json, calib/,
# and the tools/ scripts it shells out to).
set -e
cd "$(dirname "$(readlink -f "$0")")"
[ -x desktop/target/release/insight-prime-gui ] || cargo build --release --manifest-path desktop/Cargo.toml
[ -f insight-prime.json ] || { echo "no insight-prime.json — copy desktop/insight-prime.example.json and set your IPs"; exit 1; }
exec ./desktop/target/release/insight-prime-gui "$@"
