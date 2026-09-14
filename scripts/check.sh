#!/bin/sh
set -eu
cargo fmt --all -- --check
cargo test --workspace
python3 scripts/lint_readme.py

