#!/bin/bash
cd "$(dirname "$0")/.." || exit 1
cargo clippy --workspace --all-targets --all-features -- -D warnings > tools/clippy_output.txt 2>&1
echo "EXIT_CODE=$?"
