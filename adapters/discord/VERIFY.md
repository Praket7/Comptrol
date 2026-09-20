# Verification contract

Every mutation is verified by a live Discord readback, never by request success alone. Each readback returns the resulting message id, author (id/username/bot), target (channel_id/guild_id), and content-hash:

- `discord.message.draft`: no POST. Verifier is the bound channel (`GET /channels/{id}`) plus the returned `draft_id`, `channel_id`, `guild_id`, `content_hash`, and `nonce` with `verification=discord_draft_local`.
- `discord.message.send`: POST with `nonce`/`enforce_nonce=true`, then `GET /channels/{channel_id}/messages/{id}`. Verifier is `message_id`, `author_id`/`author_username`, `channel_id`/`guild_id`, `content_hash`, and `nonce` with `verification=discord_message_readback`.
- `discord.message.edit`: PATCH then GET the same id. Verifier is `message_id`, author, target, and the new `content_hash` with `verification=discord_edit_readback`; mismatch raises `reconcile_failed`.
- `discord.message.delete`: DELETE then GET expecting 404. Verifier is `message_id`, prior author/target, and `deleted=true` with `verification=discord_delete_readback`.
- `discord.message.reply`: parent GET in the same channel, POST with `message_reference`, then GET the new id. Verifier is `message_id`, `reply_to_message_id`, author, target, and `content_hash` with `verification=discord_reply_readback`.
- `discord.message.react`: PUT `.../reactions/{emoji}/@me` then GET the message. Verifier is `message_id`, author, target, `emoji`, and `reaction_present=true` with `verification=discord_reaction_readback`.
- `discord.message.attach`: multipart POST from a file under `COMPTROL_DISCORD_ASSETS_ROOT` (8 MB cap), then GET the new id. Verifier is `message_id`, author, target, `filename`, `size_bytes`, `sha256`, and `content_hash` with `verification=discord_attachment_readback`.
- `discord.message.search`: `GET /channels/{channel_id}/messages` with `limit`/`before`/`after`/`around`. Verifier is the live list of `{message_id, author_id, author_username, content_excerpt, content_hash}` with `verification=discord_history_readback`.

Refusals are verified without network writes: a `use_user_token`/`auth_mode=user` payload returns `selfbot_refused`; an `as_user`/`account=user` payload returns `user_account_ui_route`; a transport failure during POST returns `ambiguous_ack` (reconcile via `discord.message.search` first, never blind retry).

Verify with: `python3.11 -m py_compile src/adapter.py`, a handshake frame through stdio expecting `comptrol.discord`, a capabilities frame listing all eight intents, a draft against a loopback stub via `COMPTROL_DISCORD_API_BASE` asserting the bound `channel_id`/`content_hash`/`nonce` with no POST issued, a send asserting the returned `message_id`/`author_id`/`channel_id`/`content_hash` match a subsequent GET, a delete asserting the follow-up GET is 404, an attach asserting the on-disk bytes hash to the reported `sha256`, and refusal frames asserting `selfbot_refused` and `user_account_ui_route` codes.
