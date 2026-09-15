# Comptrol V4 baseline

Date: 2026-09-15

Branch: `feat/comptrol-v4-fast-verified`

Base SHA: `0f6612a3a0d65e73ccf3181ce7b589961d59c47a`

Repository: `Praket7/Comptrol`

Environment:

```text
OS: Windows host with WSL build environment
Rust: rustc 1.98.1 (48a229cea 2026-09-01)
Cargo: 1.98.1 (797e8a9bc 2026-08-05)
Node: v22.22.1
npm: 9.2.0
Chrome: 153.0.8010.36
Server version: 0.1.32
```

Baseline commands:

```text
cargo test --workspace --all-targets: passed; 43 Rust tests plus adapter/platform targets
cargo fmt --all -- --check: passed on the prior release head
cargo clippy --workspace --all-targets -- -D warnings: passed on the prior release head
cargo build --workspace --release: pending for V4 changes
node scripts/browser_conformance.mjs: passed on the prior release head; 20 calls, one browser websocket, one page websocket
adapter conformance: present; live application acceptance is host-dependent
CI state: pending for this new branch
```

The three pre-existing untracked paths `node_modules/`, `pnpm-lock.yaml`, and `work/` are preserved and are not baseline source changes.

V4 baseline priorities are browser concurrency/state extraction, independent verification, current workflow fingerprints and parameter lifting, protocol-compatible transport separation, and route/adapter reliability evidence. The former Chrome closed-group extension is comparison code only under `experiments/chrome-closed-groups-extension`; normal packaging does not install it.
