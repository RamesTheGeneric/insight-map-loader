#!/bin/bash
# Cross-compile the memread helper for the Quest 1 (aarch64). Uses the vendored
# NDK if present, else the first aarch64 clang on PATH.
set -e
cd "$(dirname "$(readlink -f "$0")")"
NDK="$HOME/Documents/GitHub/Q2Slam/ndk/android-ndk-r27c/toolchains/llvm/prebuilt/linux-x86_64/bin"
CC="$NDK/aarch64-linux-android29-clang"
[ -x "$CC" ] || CC=$(command -v aarch64-linux-android29-clang || true)
[ -x "$CC" ] || { echo "no aarch64 android clang found (set NDK)"; exit 1; }
"$CC" -O2 memread.c -o memread
echo "built ./memread ($(stat -c%s memread) bytes)"
