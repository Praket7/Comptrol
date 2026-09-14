#!/bin/sh
set -eu
cargo fmt --all -- --check
cargo test --workspace
cargo build
python3 scripts/lint_readme.py
npm run test:browser
python3 scripts/client_conformance.py
python3 scripts/generate_homebrew_formula.py --version 0.1.0 --url https://example.invalid/comptrol.tar.gz --sha256 0000000000000000000000000000000000000000000000000000000000000000 --output "$(mktemp -d)/comptrol.rb"
python3 scripts/benchmark.py --iterations 10
