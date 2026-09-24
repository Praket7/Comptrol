# Security fixes from the September audit

This record follows the four reproduced findings in the attached audit. The fixes were made at the shared runtime boundary.

## Caller supplied risk

The runtime now uses the higher of the intent's server classification and the caller supplied risk. A caller can request stricter handling. It cannot lower a mutation to read only. A regression check confirms that an R0 write stays blocked while the stop latch is active.

## Consent store errors

Runtime startup keeps running when the consent file cannot be read. Consent gated actions remain blocked. The doctor report now exposes the open error and says that mutations are blocked. It does not report the store as healthy.

## Sandbox links

Sandbox reads, writes, copies, backups, and restores use directory handles rooted in the Comptrol state directory. Paths must be relative. Parent traversal and backslash based traversal are rejected. Symlink escape checks confirm that outside files remain untouched. Windows junction behavior still needs a live Windows run.

## Reused idempotency keys

Each durable receipt stores a hash of the canonical request. An exact retry returns the saved result, including after restart. A changed request with the same key returns `idempotency_conflict`. Older receipts without a request hash cannot be replayed under a key.

## Adapter readiness

The doctor report now separates environment availability, local policy permission, attempts, verified successes, and routes never tested in this runtime. Availability does not claim that an external app is installed. A prior verified result is evidence from this runtime, not proof that a future operation will work.

## Boundaries still needing evidence

The current test run is local to this macOS checkout. It does not prove Windows junction resistance, Linux desktop control, live adapter coverage, signed installer behavior, or hosted ChatGPT connectivity. Remote HTTP deployment remains outside the default local setup. Do not expose the loopback service directly to the internet.
