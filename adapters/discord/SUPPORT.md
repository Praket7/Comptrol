# Support boundary

Supported: channel-bound drafts (no POST), bot sends with nonce plus readback, edits with content-hash readback, deletes with 404 readback, replies bound to a parent in the same channel, single-emoji reactions with readback, file attachments from `COMPTROL_DISCORD_ASSETS_ROOT` (8 MB cap, sha256 recorded), and channel history queries with bounded excerpts plus client-side author/content filters.

Unsupported: self-bots and any user-token route (`selfbot_refused`), user-account sends without a bot (`user_account_ui_route`, operate the signed-in UI instead), guild-wide full-text search (OAuth-only, not a Bot API surface), embeds, stickers, polls, threads, forum posts, voice, stage, scheduled events, moderation, role or permission management, webhooks, gateway streaming, message contents beyond the 2000-character cap, attachments larger than 8 MB or outside the assets root, and blind retries after an ambiguous acknowledgement.
