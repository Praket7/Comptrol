#!/bin/sh
set -eu
cargo fmt --all -- --check
cargo test --workspace
cargo build
python3 scripts/lint_readme.py
npm run test:browser
npm run test:launcher
npm run test:daemon-launcher
python3 scripts/client_conformance.py
python3 scripts/progress_conformance.py
python3 scripts/tasks_conformance.py
python3 scripts/recovery_conformance.py
python3 scripts/release_conformance.py
python3 scripts/pairing_conformance.py
python3 scripts/platform_conformance.py
python3 scripts/privacy_conformance.py
python3 scripts/command_conformance.py
python3 scripts/browser_launcher_conformance.py
node scripts/chrome_auto_connect_conformance.mjs
python3 scripts/http_conformance.py
python3 scripts/ipc_conformance.py
python3 scripts/chrome_conformance.py
python3 scripts/visible_chrome_conformance.py
python3 scripts/macos_ax_conformance.py
python3 scripts/macos_app_conformance.py
python3 scripts/windows_uia_conformance.py
python3 scripts/linux_atspi_conformance.py
python3 scripts/adapter_conformance.py
python3 scripts/adapter_runtime_conformance.py
python3 scripts/native_browser_conformance.py
python3 scripts/macos_chrome_group_conformance.py
python3 scripts/check_versions.py
python3 scripts/generate_homebrew_formula.py --version "$(cat VERSION)" --url https://example.invalid/comptrol.tar.gz --sha256 0000000000000000000000000000000000000000000000000000000000000000 --output "$(mktemp -d)/comptrol.rb"
python3 scripts/benchmark.py --iterations 10
