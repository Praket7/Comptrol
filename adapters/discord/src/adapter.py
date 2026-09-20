#!/usr/bin/env python3
"""Discord adapter: messaging over the official Discord Bot API only.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

Upstream is the official Discord Bot HTTP API only
(https://discord.com/api/v10), using the Python standard library (urllib).
There is no browser automation, no gateway scraping, and no user-token flow
anywhere in this adapter.

Auth: the bot token is taken ONLY from the COMPTROL_DISCORD_BOT_TOKEN
environment variable at request time. It is never stored on disk, never
logged, never echoed, and never copied into responses, errors, or audit
payloads. Upstream error text is redacted before it leaves the adapter.

Self-bot refusal: any payload requesting a user-token route (auth_mode=user,
token_type=user, use_user_token, or an embedded user_token) is refused with
code selfbot_refused without touching the network.

User accounts: acting as a normal signed-in account (as_user/account=user
without a bot) is refused with code user_account_ui_route with guidance to
operate the signed-in Discord UI. No token automation is implemented.

Identity binding: every intent binds the exact channel via
GET /channels/{channel_id} before acting. An optional guild_id must equal the
channel's guild_id; an optional dm_user_id must appear in a DM channel's
recipients. Sends never resolve names to ids.

Idempotency: every send/reply/attach carries a client-generated nonce with
enforce_nonce=true. After every POST the adapter GETs the message id and
returns author/target/content-hash. An ambiguous POST outcome returns code
ambiguous_ack with reconcile-before-retry guidance; the adapter never blind
retries.

Attachments: the local file must exist under COMPTROL_DISCORD_ASSETS_ROOT,
must stay inside that root, is capped at 8 MB, and its sha256 is recorded.
"""

import hashlib
import json
import mimetypes
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.discord"
TOKEN_ENV = "COMPTROL_DISCORD_BOT_TOKEN"
ASSETS_ROOT_ENV = "COMPTROL_DISCORD_ASSETS_ROOT"
API_BASE_ENV = "COMPTROL_DISCORD_API_BASE"

DEFAULT_API_BASE = "https://discord.com/api/v10"

HTTP_TIMEOUT_S = 20.0
MAX_JSON_BYTES = 4 * 1024 * 1024
READ_CHUNK = 65536
MAX_CONTENT_CHARS = 2000
MAX_EMOJI_CHARS = 64
MAX_SEARCH_LIMIT = 100
MAX_EXCERPT_CHARS = 500
ATTACH_MAX_BYTES = 8 * 1024 * 1024
MAX_NONCE_CHARS = 64

INTENTS = (
    "discord.message.draft",
    "discord.message.send",
    "discord.message.edit",
    "discord.message.delete",
    "discord.message.reply",
    "discord.message.react",
    "discord.message.attach",
    "discord.message.search",
)

_NONCE_CHARS = frozenset(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
)


class UpstreamError(RuntimeError):
    """Upstream or credential failure with a safe, token-free detail string."""

    def __init__(self, code, detail):
        super().__init__(detail)
        self.code = code
        self.detail = detail


def _redact(text, token):
    if token and text and token in text:
        return text.replace(token, "[redacted]")
    return text


def _token():
    token = os.environ.get(TOKEN_ENV, "")
    if not isinstance(token, str) or not token.strip():
        raise UpstreamError("auth_missing", TOKEN_ENV + " is not set")
    return token.strip()


def _api_base():
    base = os.environ.get(API_BASE_ENV, DEFAULT_API_BASE)
    if not isinstance(base, str) or not base.strip():
        return DEFAULT_API_BASE
    return base.strip().rstrip("/") or DEFAULT_API_BASE


def _refuse_forbidden_routes(payload):
    """Refuse self-bot and user-account automation before any network use."""
    if not isinstance(payload, dict):
        return
    # Any user-token route is a self-bot attempt: refuse explicitly.
    if (
        payload.get("use_user_token") is True
        or payload.get("user_token") is not None
        or payload.get("token_type") == "user"
        or payload.get("auth_mode") == "user"
    ):
        raise UpstreamError(
            "selfbot_refused",
            "user tokens / self-bots are refused; this adapter uses only the "
            "official Bot API with " + TOKEN_ENV,
        )
    # A normal user account (no bot) is never automated here.
    if payload.get("as_user") is True or payload.get("account") in (
        "user",
        "personal",
        "user_account",
    ):
        raise UpstreamError(
            "user_account_ui_route",
            "user-account sends are not automated; operate the signed-in "
            "Discord UI for that account instead (no token automation is "
            "implemented)",
        )


def _from_http_error(exc, token):
    try:
        raw = exc.read(4096)
    except Exception:
        raw = b""
    snippet = _redact(raw.decode("utf-8", errors="replace"), token)
    message = snippet[:500]
    try:
        parsed = json.loads(snippet)
        if isinstance(parsed, dict) and isinstance(parsed.get("message"), str):
            message = parsed["message"][:300]
    except ValueError:
        pass
    status = getattr(exc, "code", 0)
    if status in (401, 403):
        return UpstreamError(
            "auth_failed",
            "Discord rejected the bot credential (HTTP %d): %s" % (status, message[:200]),
        )
    if status == 404:
        return UpstreamError("not_found", "Discord resource not found (HTTP 404)")
    if status == 429:
        return UpstreamError(
            "rate_limited",
            "Discord rate limit hit (HTTP 429); back off and reconcile with "
            "discord.message.search before retrying: %s" % message[:200],
        )
    return UpstreamError(
        "upstream_error", "Discord API HTTP %d: %s" % (status, message[:200])
    )


def _api_request(method, path, token, body=None, query=None):
    """Low-level Bot API call returning (status, parsed_json)."""
    base = _api_base()
    url = base + path
    if query:
        url += "?" + urllib.parse.urlencode(query)
    data = None
    headers = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json; charset=utf-8"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    req.add_header("Authorization", "Bot " + token)
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
            status = getattr(resp, "status", 200)
            raw = resp.read(MAX_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise _from_http_error(exc, token)
    except urllib.error.URLError as exc:
        reason = _redact(str(getattr(exc, "reason", exc)), token)[:200]
        # The request may or may not have landed: caller decides whether the
        # outcome is ambiguous (POST) or a plain transport failure (GET/etc).
        raise UpstreamError("upstream_unreachable", "Discord unreachable: " + reason)
    except TimeoutError as exc:
        raise UpstreamError(
            "upstream_unreachable",
            "Discord request timed out: " + _redact(str(exc), token)[:200],
        )
    if len(raw) > MAX_JSON_BYTES:
        raise UpstreamError("response_too_large", "upstream JSON exceeded the read cap")
    if not raw:
        return status, {}
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise UpstreamError("upstream_error", "upstream returned non-JSON content")
    return status, parsed


def _api_json(method, path, token, body=None, query=None):
    _, parsed = _api_request(method, path, token, body=body, query=query)
    return parsed


def _api_post_message(channel_id, token, body):
    """POST a message; network failures here are ambiguous (may have landed)."""
    try:
        _, parsed = _api_request(
            "POST",
            "/channels/" + channel_id + "/messages",
            token,
            body=body,
        )
    except UpstreamError as exc:
        if exc.code == "upstream_unreachable":
            raise UpstreamError(
                "ambiguous_ack",
                "POST outcome unknown (transport failed after send); do NOT "
                "blind retry with a new nonce; call discord.message.search on "
                "this channel to reconcile by nonce/content-hash, then retry "
                "only if the message is absent",
            )
        raise
    if not isinstance(parsed, dict) or not parsed.get("id"):
        raise UpstreamError("ambiguous_ack", _ambiguous_guidance())
    return parsed


def _ambiguous_guidance():
    return (
        "ambiguous acknowledgement from Discord; do NOT blind retry; call "
        "discord.message.search on this channel to reconcile by nonce / "
        "content-hash, then retry only if the message is absent"
    )


def _api_multipart(path, token, payload_json, filename, file_bytes, content_type):
    """Multipart POST for file uploads, stdlib only."""
    boundary = "comptrol" + uuid.uuid4().hex
    body = bytearray()

    def _field(name, value, ctype="application/json"):
        body.extend(("--" + boundary + "\r\n").encode())
        body.extend(
            ('Content-Disposition: form-data; name="%s"\r\n' % name).encode()
        )
        body.extend(("Content-Type: " + ctype + "\r\n\r\n").encode())
        if isinstance(value, bytes):
            body.extend(value)
        else:
            body.extend(str(value).encode("utf-8"))
        body.extend(b"\r\n")

    _field("payload_json", json.dumps(payload_json))
    body.extend(("--" + boundary + "\r\n").encode())
    body.extend(
        ('Content-Disposition: form-data; name="files[0]"; filename="%s"\r\n'
         % filename).encode()
    )
    body.extend(("Content-Type: " + content_type + "\r\n\r\n").encode())
    body.extend(file_bytes)
    body.extend(("\r\n--" + boundary + "--\r\n").encode())

    url = _api_base() + path
    req = urllib.request.Request(
        url,
        data=bytes(body),
        headers={
            "Accept": "application/json",
            "Content-Type": "multipart/form-data; boundary=" + boundary,
        },
        method="POST",
    )
    req.add_header("Authorization", "Bot " + token)
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
            raw = resp.read(MAX_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise _from_http_error(exc, token)
    except urllib.error.URLError as exc:
        raise UpstreamError(
            "ambiguous_ack",
            "multipart POST outcome unknown (transport failed after send); do "
            "NOT blind retry with a new nonce; call discord.message.search to "
            "reconcile, then retry only if absent: "
            + _redact(str(getattr(exc, "reason", exc)), token)[:150],
        )
    if len(raw) > MAX_JSON_BYTES:
        raise UpstreamError("response_too_large", "upstream JSON exceeded the read cap")
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise UpstreamError("upstream_error", "upstream returned non-JSON content")
    if not isinstance(parsed, dict) or not parsed.get("id"):
        raise UpstreamError("ambiguous_ack", _ambiguous_guidance())
    return parsed


# ---------------------------------------------------------------- validators


def _req_snowflake(payload, key):
    value = payload.get(key)
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 32
        or not value.isdigit()
    ):
        raise ValueError(key + " must be a Discord snowflake (1-32 digits)")
    return value


def _opt_snowflake(payload, key):
    value = payload.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not 1 <= len(value) <= 32 or not value.isdigit():
        raise ValueError(key + " must be a Discord snowflake (1-32 digits)")
    return value


def _req_str(payload, key, max_len, allow_empty=False):
    value = payload.get(key)
    if (
        not isinstance(value, str)
        or len(value) > max_len
        or (not allow_empty and not value)
    ):
        if allow_empty:
            raise ValueError(key + " must be a string of length 0-%d" % max_len)
        raise ValueError(key + " must be a string of length 1-%d" % max_len)
    return value


def _opt_str(payload, key, max_len, default=None):
    if key not in payload or payload.get(key) is None:
        return default
    value = payload.get(key)
    if not isinstance(value, str) or len(value) > max_len:
        raise ValueError(key + " must be a string of length 0-%d" % max_len)
    return value


def _req_int(payload, key, minimum, maximum):
    value = payload.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(key + " must be an integer in [%d, %d]" % (minimum, maximum))
    if not minimum <= value <= maximum:
        raise ValueError(key + " must be an integer in [%d, %d]" % (minimum, maximum))
    return value


def _opt_nonce(payload):
    value = payload.get("nonce")
    if value is None:
        return uuid.uuid4().hex
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= MAX_NONCE_CHARS
        or any(c not in _NONCE_CHARS for c in value)
    ):
        raise ValueError(
            "nonce must be a client-generated string of 1-%d chars [A-Za-z0-9-_]"
            % MAX_NONCE_CHARS
        )
    return value


def _content_hash(content):
    return hashlib.sha256(content.encode("utf-8")).hexdigest()


# ---------------------------------------------------------------- identity


def _bind_channel(channel_id, token, expected_guild_id=None, expected_dm_user_id=None):
    """GET the channel and enforce exact guild/DM identity binding."""
    channel = _api_json("GET", "/channels/" + channel_id, token)
    if not isinstance(channel, dict) or channel.get("id") != channel_id:
        raise UpstreamError("identity_mismatch", "channel binding failed: id mismatch")
    if expected_guild_id is not None:
        actual_guild = channel.get("guild_id")
        if actual_guild != expected_guild_id:
            raise UpstreamError(
                "identity_mismatch",
                "channel %s is not in guild %s" % (channel_id, expected_guild_id),
            )
    if expected_dm_user_id is not None:
        recipients = channel.get("recipients", [])
        ids = [
            r.get("id") for r in recipients if isinstance(r, dict) and r.get("id")
        ]
        if channel.get("type") != 1 or expected_dm_user_id not in ids:
            raise UpstreamError(
                "identity_mismatch",
                "DM channel %s is not bound to user %s"
                % (channel_id, expected_dm_user_id),
            )
    return channel


def _summarize_message(message):
    if not isinstance(message, dict):
        raise UpstreamError("upstream_error", "Discord returned an unexpected message")
    author = message.get("author", {}) if isinstance(message.get("author"), dict) else {}
    return {
        "message_id": str(message.get("id", "")),
        "channel_id": str(message.get("channel_id", "")),
        "guild_id": str(message.get("guild_id", "") or ""),
        "author_id": str(author.get("id", "")),
        "author_username": author.get("username", "")
        if isinstance(author.get("username"), str)
        else "",
        "author_bot": bool(author.get("bot", False)),
        "content": message.get("content", "")
        if isinstance(message.get("content"), str)
        else "",
        "timestamp": message.get("timestamp", "")
        if isinstance(message.get("timestamp"), str)
        else "",
    }


def _reconcile_message(channel_id, message_id, token, expected_content=None):
    """GET the message id back and return author/target/content-hash."""
    fetched = _api_json(
        "GET", "/channels/" + channel_id + "/messages/" + message_id, token
    )
    summary = _summarize_message(fetched)
    if summary["message_id"] != message_id or summary["channel_id"] != channel_id:
        raise UpstreamError(
            "identity_mismatch", "reconciliation id/channel mismatch after write"
        )
    content = expected_content if expected_content is not None else summary["content"]
    summary["content"] = content
    summary["content_hash"] = _content_hash(content)
    return summary


def _get_message_or_none(channel_id, message_id, token):
    try:
        return _api_json(
            "GET", "/channels/" + channel_id + "/messages/" + message_id, token
        )
    except UpstreamError as exc:
        if exc.code == "not_found":
            return None
        raise


# ---------------------------------------------------------------- handlers


def handle_draft(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    content = _req_str(payload, "content", MAX_CONTENT_CHARS)
    guild_id = _opt_snowflake(payload, "guild_id")
    dm_user_id = _opt_snowflake(payload, "dm_user_id")
    channel = _bind_channel(channel_id, token, guild_id, dm_user_id)
    nonce = _opt_nonce(payload)
    return {
        "draft_id": "draft-" + uuid.uuid4().hex[:12],
        "channel_id": channel_id,
        "guild_id": str(channel.get("guild_id", "") or ""),
        "channel_type": channel.get("type"),
        "content": content,
        "content_hash": _content_hash(content),
        "nonce": nonce,
        "persisted": False,
        "verified": True,
        "verification": "discord_draft_local",
        "note": "draft only; nothing was posted upstream",
    }


def handle_send(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    content = _req_str(payload, "content", MAX_CONTENT_CHARS)
    guild_id = _opt_snowflake(payload, "guild_id")
    dm_user_id = _opt_snowflake(payload, "dm_user_id")
    _bind_channel(channel_id, token, guild_id, dm_user_id)
    nonce = _opt_nonce(payload)
    created = _api_post_message(
        channel_id,
        token,
        {"content": content, "nonce": nonce, "enforce_nonce": True},
    )
    created_summary = _summarize_message(created)
    if created_summary["channel_id"] != channel_id:
        raise UpstreamError("identity_mismatch", "send landed in the wrong channel")
    reconciled = _reconcile_message(channel_id, created_summary["message_id"], token)
    if reconciled["content"] != content:
        raise UpstreamError(
            "reconcile_failed", "post-send GET content does not match request"
        )
    reconciled["nonce"] = nonce
    reconciled["verified"] = True
    reconciled["verification"] = "discord_message_readback"
    return reconciled


def handle_edit(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    message_id = _req_snowflake(payload, "message_id")
    content = _req_str(payload, "content", MAX_CONTENT_CHARS)
    guild_id = _opt_snowflake(payload, "guild_id")
    _bind_channel(channel_id, token, guild_id)
    _api_json(
        "PATCH",
        "/channels/" + channel_id + "/messages/" + message_id,
        token,
        body={"content": content},
    )
    reconciled = _reconcile_message(channel_id, message_id, token)
    verified = reconciled["content"] == content
    reconciled["content_hash"] = _content_hash(reconciled["content"])
    reconciled["verified"] = verified
    reconciled["verification"] = "discord_edit_readback"
    if not verified:
        raise UpstreamError("reconcile_failed", "post-edit GET content mismatch")
    return reconciled


def handle_delete(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    message_id = _req_snowflake(payload, "message_id")
    guild_id = _opt_snowflake(payload, "guild_id")
    _bind_channel(channel_id, token, guild_id)
    before = _get_message_or_none(channel_id, message_id, token)
    before_summary = _summarize_message(before) if before is not None else None
    _api_json("DELETE", "/channels/" + channel_id + "/messages/" + message_id, token)
    after = _get_message_or_none(channel_id, message_id, token)
    deleted = after is None
    body = {
        "message_id": message_id,
        "channel_id": channel_id,
        "guild_id": before_summary["guild_id"] if before_summary else "",
        "author_id": before_summary["author_id"] if before_summary else "",
        "author_username": before_summary["author_username"] if before_summary else "",
        "deleted": deleted,
        "verified": deleted,
        "verification": "discord_delete_readback",
    }
    if not deleted:
        raise UpstreamError("reconcile_failed", "message still present after DELETE")
    return body


def handle_reply(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    content = _req_str(payload, "content", MAX_CONTENT_CHARS)
    reply_to = _req_snowflake(payload, "reply_to_message_id")
    guild_id = _opt_snowflake(payload, "guild_id")
    channel = _bind_channel(channel_id, token, guild_id)
    parent = _get_message_or_none(channel_id, reply_to, token)
    if parent is None:
        raise UpstreamError("not_found", "reply target message not found in channel")
    nonce = _opt_nonce(payload)
    body = {
        "content": content,
        "nonce": nonce,
        "enforce_nonce": True,
        "message_reference": {
            "message_id": reply_to,
            "channel_id": channel_id,
        },
    }
    if channel.get("guild_id"):
        body["message_reference"]["guild_id"] = channel.get("guild_id")
    created = _api_post_message(channel_id, token, body)
    created_summary = _summarize_message(created)
    reconciled = _reconcile_message(channel_id, created_summary["message_id"], token)
    if reconciled["content"] != content:
        raise UpstreamError(
            "reconcile_failed", "post-reply GET content does not match request"
        )
    reconciled["reply_to_message_id"] = reply_to
    reconciled["nonce"] = nonce
    reconciled["verified"] = True
    reconciled["verification"] = "discord_reply_readback"
    return reconciled


def handle_react(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    message_id = _req_snowflake(payload, "message_id")
    emoji = _req_str(payload, "emoji", MAX_EMOJI_CHARS)
    if "/" in emoji or "\\" in emoji:
        raise ValueError("emoji must be a single unicode emoji or name:id")
    guild_id = _opt_snowflake(payload, "guild_id")
    _bind_channel(channel_id, token, guild_id)
    target = _get_message_or_none(channel_id, message_id, token)
    if target is None:
        raise UpstreamError("not_found", "reaction target message not found")
    encoded = urllib.parse.quote(emoji, safe="")
    _api_json(
        "PUT",
        "/channels/" + channel_id + "/messages/" + message_id
        + "/reactions/" + encoded + "/@me",
        token,
    )
    fetched = _get_message_or_none(channel_id, message_id, token)
    summary = _summarize_message(fetched) if fetched is not None else None
    if summary is None:
        raise UpstreamError("reconcile_failed", "message vanished after react")
    reactions = fetched.get("reactions", []) if isinstance(fetched, dict) else []
    found = False
    if isinstance(reactions, list):
        for entry in reactions:
            if not isinstance(entry, dict):
                continue
            info = entry.get("emoji", {})
            name = info.get("name") if isinstance(info, dict) else None
            ident = info.get("id") if isinstance(info, dict) else None
            if emoji == name or emoji == ("%s:%s" % (name, ident)):
                if entry.get("me"):
                    found = True
                    break
                if entry.get("count", 0) >= 1 and name == emoji:
                    found = True
                    break
    return {
        "message_id": message_id,
        "channel_id": channel_id,
        "guild_id": summary["guild_id"],
        "author_id": summary["author_id"],
        "author_username": summary["author_username"],
        "content_hash": _content_hash(summary["content"]),
        "emoji": emoji,
        "reaction_present": found,
        "verified": found,
        "verification": "discord_reaction_readback",
    }


def _resolve_asset(file_ref):
    root = os.environ.get(ASSETS_ROOT_ENV, "")
    if not root or not os.path.isabs(root):
        raise UpstreamError(
            "assets_root_missing",
            ASSETS_ROOT_ENV + " must be set to an absolute directory",
        )
    if not isinstance(file_ref, str) or not file_ref or len(file_ref) > 512:
        raise ValueError("file must be a relative path of length 1-512")
    if file_ref.startswith("/") or ".." in Path(file_ref).parts:
        raise ValueError("file must stay inside " + ASSETS_ROOT_ENV)
    candidate = os.path.realpath(os.path.join(root, file_ref))
    real_root = os.path.realpath(root)
    if candidate != real_root and not candidate.startswith(real_root + os.sep):
        raise ValueError("file must stay inside " + ASSETS_ROOT_ENV)
    if not os.path.isfile(candidate):
        raise UpstreamError("attachment_missing", "local file not found under assets root")
    size = os.path.getsize(candidate)
    if size > ATTACH_MAX_BYTES:
        raise UpstreamError(
            "attachment_too_large",
            "attachment exceeds the 8 MB cap (%d bytes)" % size,
        )
    with open(candidate, "rb") as handle:
        file_bytes = handle.read(ATTACH_MAX_BYTES + 1)
    if len(file_bytes) > ATTACH_MAX_BYTES:
        raise UpstreamError("attachment_too_large", "attachment exceeds the 8 MB cap")
    digest = hashlib.sha256(file_bytes).hexdigest()
    filename = os.path.basename(candidate)
    content_type, _ = mimetypes.guess_type(filename)
    return filename, file_bytes, size, digest, content_type or "application/octet-stream"


def handle_attach(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    file_ref = payload.get("file")
    content = _opt_str(payload, "content", MAX_CONTENT_CHARS, default="")
    if content is None:
        content = ""
    guild_id = _opt_snowflake(payload, "guild_id")
    dm_user_id = _opt_snowflake(payload, "dm_user_id")
    if not content and file_ref is None:
        raise ValueError("attach requires file, or non-empty content with file")
    if file_ref is None:
        raise ValueError("file is required for discord.message.attach")
    _bind_channel(channel_id, token, guild_id, dm_user_id)
    filename, file_bytes, size, digest, content_type = _resolve_asset(file_ref)
    nonce = _opt_nonce(payload)
    payload_json = {
        "content": content,
        "nonce": nonce,
        "enforce_nonce": True,
    }
    created = _api_multipart(
        "/channels/" + channel_id + "/messages",
        token,
        payload_json,
        filename,
        file_bytes,
        content_type,
    )
    created_summary = _summarize_message(created)
    reconciled = _reconcile_message(channel_id, created_summary["message_id"], token)
    reconciled["filename"] = filename
    reconciled["size_bytes"] = size
    reconciled["sha256"] = digest
    reconciled["nonce"] = nonce
    attachments = created.get("attachments", [])
    reconciled["attachment_count"] = len(attachments) if isinstance(attachments, list) else 0
    reconciled["verified"] = True
    reconciled["verification"] = "discord_attachment_readback"
    return reconciled


def handle_search(payload, token):
    channel_id = _req_snowflake(payload, "channel_id")
    guild_id = _opt_snowflake(payload, "guild_id")
    _bind_channel(channel_id, token, guild_id)
    limit = payload.get("limit", 25)
    if isinstance(limit, bool) or not isinstance(limit, int):
        raise ValueError("limit must be an integer in [1, 100]")
    if not 1 <= limit <= MAX_SEARCH_LIMIT:
        raise ValueError("limit must be an integer in [1, 100]")
    query = {"limit": limit}
    for key in ("before", "after", "around"):
        value = payload.get(key)
        if value is not None:
            if (
                not isinstance(value, str)
                or not 1 <= len(value) <= 32
                or not value.isdigit()
            ):
                raise ValueError(key + " must be a Discord snowflake (1-32 digits)")
            query[key] = value
    author_filter = _opt_snowflake(payload, "author_id")
    content_contains = _opt_str(payload, "content_contains", 500, default=None)
    listing = _api_json(
        "GET", "/channels/" + channel_id + "/messages", token, query=query
    )
    if not isinstance(listing, list):
        raise UpstreamError("upstream_error", "history query returned an unexpected shape")
    items = []
    for entry in listing:
        if not isinstance(entry, dict):
            continue
        summary = _summarize_message(entry)
        if author_filter is not None and summary["author_id"] != author_filter:
            continue
        if content_contains is not None and content_contains not in summary["content"]:
            continue
        full = summary.pop("content")
        summary["content_excerpt"] = full[:MAX_EXCERPT_CHARS]
        summary["content_hash"] = _content_hash(full)
        summary["excerpt_truncated"] = len(full) > MAX_EXCERPT_CHARS
        items.append(summary)
        if len(items) >= limit:
            break
    return {
        "channel_id": channel_id,
        "count": len(items),
        "messages": items,
        "verified": True,
        "verification": "discord_history_readback",
        "note": "bot-scope channel history with client-side filters; guild-wide "
        "full-text search is OAuth-only and unsupported",
    }


# ---------------------------------------------------------------- dispatch

_DISPATCH = {
    "discord.message.draft": handle_draft,
    "discord.message.send": handle_send,
    "discord.message.edit": handle_edit,
    "discord.message.delete": handle_delete,
    "discord.message.reply": handle_reply,
    "discord.message.react": handle_react,
    "discord.message.attach": handle_attach,
    "discord.message.search": handle_search,
}


def handler(request):
    method = request.get("method")
    if method == "handshake":
        return response(
            request, True, "available", {"adapter": ADAPTER_ID, "protocol": "discord-bot-v10"}
        )
    if method == "capabilities":
        return response(
            request,
            True,
            "available",
            {"backend": "discord-bot-api-v10", "intents": list(INTENTS)},
        )
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload")
    if not isinstance(payload, dict):
        return response(
            request,
            False,
            "degraded",
            error={"code": "invalid_payload", "message": "payload must be an object"},
        )
    intent = payload.get("intent")
    func = _DISPATCH.get(intent)
    if func is None:
        return response(
            request,
            False,
            "unsupported",
            error={"code": "unsupported_intent", "message": str(intent)},
        )
    token = ""
    try:
        _refuse_forbidden_routes(payload)
        token = _token()
        return response(request, True, "available", func(payload, token))
    except UpstreamError as exc:
        health = (
            "unsupported"
            if exc.code
            in ("selfbot_refused", "user_account_ui_route", "unsupported_intent")
            else "degraded"
        )
        return response(
            request,
            False,
            health,
            error={"code": exc.code, "message": _redact(exc.detail, token)},
        )
    except (ValueError, TypeError, KeyError) as exc:
        return response(
            request,
            False,
            "degraded",
            error={"code": "invalid_payload", "message": _redact(str(exc), token)},
        )
    except Exception as exc:
        return response(
            request,
            False,
            "degraded",
            error={"code": "adapter_error", "message": _redact(str(exc), token)},
        )


if __name__ == "__main__":
    serve(handler)
