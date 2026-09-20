# Comptrol Canva bridge protocol — v0.1.0

Parties: (A) the user-approved Canva App iframe running the Apps SDK Design
Editing model, (B) this companion extension (content relay + background
service worker), (C) the localhost runtime bridge
(`extensions/canva/src/local_bridge.py`). All three speak the envelopes
below. Protocol version string `comptrol.canva.bridge/0.1.0` is carried in
every message; receivers reject anything else.

## 1. Transport paths

Primary path — extension relay (no PNA/CORS involved):

```
Canva App iframe --postMessage--> content_relay.js --chrome.runtime-->
background.js --native messaging--> local runtime / local_bridge.py
```

postMessage never leaves the page, and native messaging is OS IPC, not
network traffic, so neither Private Network Access preflights nor CORS apply
on this path. It is primary because it works regardless of browser PNA
enforcement.

Fallback path — direct localhost fetch from the Canva App iframe to
`http://127.0.0.1:8765`. This path is allowed only when a real-browser PNA
preflight succeeds (see §4). It is currently unvalidated in a real browser.

## 2. Origin allowlist

The only accepted web origins are:

- `https://www.canva.com`
- `https://canva.com`

The extension content relay ignores `message` events from any other origin
without acting. The local bridge refuses any `/v1/*` POST whose `Origin`
header is missing or not on this list (`403 origin_not_allowed`). The bearer
pairing token is never sent to, or accepted from, a web origin: it travels
only over native messaging / loopback request bodies.

## 3. Message envelopes (all JSON)

Every envelope carries `protocol`, a `nonce` (fresh per message, see §6),
and the exact bound `design_id`. Receivers echo `design_id` back in every
receipt so a mis-bound operation is visible before anything is applied.

### 3.1 Session open (nonce exchange)

```
iframe -> extension -> bridge  POST /v1/session/open
{ "protocol": "comptrol.canva.bridge/0.1.0",
  "token": "<pairing token>",
  "nonce": "<client random, 16-128 [A-Za-z0-9_-]>",
  "design_id": "<exact open design id>" }

bridge -> extension -> iframe
{ "ok": true, "protocol": "comptrol.canva.bridge/0.1.0",
  "session_id": "<opaque hex>", "bound_design_id": "<echo>",
  "nonce_echo": "<echo>" }
```

### 3.2 Operation envelope (typed ops only, no arbitrary JS)

```
POST /v1/op/submit
{ "protocol": "...", "token": "...", "nonce": "<fresh>",
  "session_id": "<from open>", "design_id": "<must equal binding>",
  "op": { "type": "<one of §5>", "params": { ... } },
  "expected_revision": "<optional, opaque>" }
```

The bridge validates, binds, and records the envelope in its relay log for
the runtime to drain. It does NOT execute Canva edits itself; execution
happens only inside the approved Canva App via the Design Editing API.

```
{ "ok": true, "protocol": "...",
  "receipt": { "receipt_id": "rcpt-000001", "session_id": "...",
               "design_id": "<echo>", "op_type": "...", "seq": 1 },
  "nonce_echo": "<echo>" }
```

### 3.3 Commit / sync receipts

```
POST /v1/session/sync
{ "protocol": "...", "token": "...", "nonce": "<fresh>",
  "session_id": "...", "design_id": "<must equal binding>" }

{ "ok": true, "protocol": "...", "session_id": "...",
  "design_id": "<echo>", "op_count": 2,
  "last_receipt_id": "rcpt-000002", "receipts": [ ... ] }
```

### 3.4 Close

```
POST /v1/session/close
{ "protocol": "...", "token": "...", "nonce": "<fresh>",
  "session_id": "...", "design_id": "<must equal binding>" }

{ "ok": true, "protocol": "...", "session_id": "...",
  "closed": true, "op_count": 2 }
```

### 3.5 Error envelope

```
{ "ok": false, "error": "<code>" }
```

Codes: `origin_not_allowed`, `invalid_token`, `nonce_invalid`,
`nonce_replay`, `design_invalid`, `unbound_design`, `session_unknown`,
`op_rejected`, `body_too_large`, `bad_json`, `method_not_allowed`,
`not_found`, `bridge_token_unconfigured`, `bridge_busy`, `relay_log_full`.

## 4. PNA / CORS analysis (fallback path only)

The Canva App iframe is a public secure context (`https://www.canva.com`);
`http://127.0.0.1:8765` is a private/local address. A direct `fetch()` from
the iframe to the bridge is therefore a public-to-private request, and
Chrome gates it behind a CORS preflight carrying
`Access-Control-Request-Private-Network: true`. The preflight succeeds only
if the bridge answers with exactly these response headers:

```
Access-Control-Allow-Origin: <echo of the request Origin, allowlisted only>
Vary: Origin
Access-Control-Allow-Methods: GET, POST, OPTIONS
Access-Control-Allow-Headers: Content-Type
Access-Control-Allow-Private-Network: true
Access-Control-Max-Age: 600
```

`src/local_bridge.py` implements exactly this: it answers `OPTIONS /v1/*`
with the headers above (plus the same `Access-Control-Allow-Origin`,
`Vary: Origin`, and `Access-Control-Allow-Private-Network: true` on every
real response), so a conforming browser can proceed to the POST. Notes:

- `http://127.0.0.1` is a potentially-trustworthy origin, so the https page
  is not blocked by mixed-content rules for loopback fetches; PNA preflight
  is the remaining gate.
- PNA behavior is browser-version dependent and has NOT been validated in a
  real browser for this bridge (see README checklist). Until that run
  passes, the extension-relay path (§1) is the only supported route and the
  direct-localhost path is documented fallback only.

## 5. Typed operations

Allowed `op.type` values (mirror the adapter intents that have no Connect
API equivalent):

- `design.text.update`, `design.image.insert`, `design.element.create`,
  `design.element.delete`, `design.element.group`, `design.element.inspect`

`params` is a JSON object (≤ 32 KiB, depth ≤ 4, string values ≤ 10 000
chars). Any envelope whose params contain screen-coordinate keys (`x`, `y`,
`screenX/Y`, `clientX/Y`, `pageX/Y`, `coordinates`, `boundingBox`, …) or
code/script keys (`script`, `eval`, `innerHTML`, `onclick`, `replay`, …) is
refused with `op_rejected`. There is no generic-RPC, `eval`, or script-text
operation type; anything outside the six typed ops is refused.

## 6. Nonce, token, and binding rules

- `token`: the pairing secret issued via `comptrol setup`
  (`COMPTROL_CANVA_BRIDGE_TOKEN`). Compared with `hmac.compare_digest`,
  never logged, never persisted.
- `nonce`: client random per message. The bridge marks each well-formed
  nonce single-use on first sight; any reuse is refused (`nonce_replay`),
  even if the enclosing request failed for another reason.
- `design_id`: fixed at session open; every later message must echo the
  exact same id or it is refused (`unbound_design`). Sessions hold no
  cookies, no profile data, and no bearer tokens — only the bound id,
  a sequence counter, and the relay log.
