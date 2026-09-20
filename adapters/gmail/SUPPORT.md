# Support boundary

Gmail draft creation, draft-triggered send, direct send, Gmail search queries, and message reads with bounded excerpts are supported. At most 10 attachments of at most 10 MB each are accepted per compose; larger or missing files are refused, never truncated. Only the `COMPTROL_GMAIL_ACCESS_TOKEN` credential source is supported. Raw MIME passthrough, batch/modify/label mutation, push notifications, delegated domain-wide sends beyond what the supplied token permits, and responses larger than the bounded read cap are unsupported.
