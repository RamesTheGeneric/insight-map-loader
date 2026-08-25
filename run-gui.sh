#!/usr/bin/env bash
# Launch the insight-map-loader GUI. Always runs from the repo root, because every path the
# service uses is relative to it (insight-map-loader.json, map/, transforms.json, calib/,
# and the tools/ scripts it shells out to).
set -e
cd "$(dirname "$(readlink -f "$0")")"
[ -x desktop/target/release/insight-map-loader-gui ] || cargo build --release --manifest-path desktop/Cargo.toml
[ -f insight-map-loader.json ] || { echo "no insight-map-loader.json — copy desktop/insight-map-loader.example.json and set your IPs"; exit 1; }
exec ./desktop/target/release/insight-map-loader-gui "$@"
