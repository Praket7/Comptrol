#!/bin/sh
set -eu
cargo fmt --all -- --check
cargo test --workspace
cargo build
python3 scripts/lint_readme.py
npm run test:browser
python3 scripts/client_conformance.py
