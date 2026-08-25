#!/usr/bin/env bash
# Launch the q2slam GUI. Always runs from the repo root, because every path the
# service uses is relative to it (q2slam.json, map/, transforms.json, calib/,
# and the tools/ scripts it shells out to).
set -e
cd "$(dirname "$(readlink -f "$0")")"
[ -x desktop/target/release/q2slam-gui ] || cargo build --release --manifest-path desktop/Cargo.toml
[ -f q2slam.json ] || { echo "no q2slam.json — copy desktop/q2slam.example.json and set your IPs"; exit 1; }
exec ./desktop/target/release/q2slam-gui "$@"
