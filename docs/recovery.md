# Recovery

Dispatched mutations become unknown on restart. Unknown results are not loaded into the successful idempotency cache, so a retry cannot be mistaken for a completed replay. Filesystem writes, sandbox copies, browser downloads, and browser fixture submissions reconcile from observed local state. Reconciliation records a new durable reconciled state and never repeats the original mutation.

Every permitted mutation is written to the local operation journal before dispatch. The journal records prepared and dispatched state without storing typed content.

If the process restarts after dispatch and before completion, a repeated idempotency key returns operation unknown. The runtime will not repeat the mutation. Call reconcile after observing the target state.

Sandbox file writes can reconcile by comparing a local path and content fingerprint. Mac accessibility actions and notifications remain unknown after a crash because the operating system does not provide a safe general effect query for those routes.

Sandbox writes create a local checkpoint before mutation. A human enabled sandbox policy can restore a checkpoint through `restore_checkpoint`. Checkpoint data stays inside the Comptrol state directory.
