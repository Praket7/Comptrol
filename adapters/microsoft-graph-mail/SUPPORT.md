# Support boundary

Graph draft creation (with inline file attachments), draft-triggered send, direct sendMail with saveToSentItems, message search, and message reads with bounded excerpts are supported. At most 10 attachments of at most 10 MB each are accepted per compose; larger or missing files are refused, never truncated. Only the `COMPTROL_GRAPH_ACCESS_TOKEN` credential source is supported. Shared mailboxes, send-on-behalf, categories/flags/rules management, delta tracking, and responses larger than the bounded read cap are unsupported.
