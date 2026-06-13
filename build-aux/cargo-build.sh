#!/usr/bin/env bash
set -euo pipefail

SOURCE_ROOT="$1"
TARGET_DIR="$2"
RUST_TARGET="$3"
OUTPUT="$4"

export CARGO_TARGET_DIR="$TARGET_DIR"

if [ "$RUST_TARGET" = "release" ]; then
    cargo build --manifest-path "$SOURCE_ROOT/Cargo.toml" --release
    cp "$TARGET_DIR/release/cycle" "$OUTPUT"
else
    cargo build --manifest-path "$SOURCE_ROOT/Cargo.toml"
    cp "$TARGET_DIR/debug/cycle" "$OUTPUT"
fi
