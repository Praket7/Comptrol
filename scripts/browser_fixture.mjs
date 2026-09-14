import { createServer } from "node:http"
import { readFile } from "node:fs/promises"
import { join } from "node:path"
import { createHash } from "node:crypto"

const port = Number(process.env.COMPTROL_FIXTURE_PORT || 17417)
const targetId = "comptrol-fixture-page"
const browserContextId = "comptrol-fixture-context"
const revision = "fixture-revision-1"
const submissions = new Map()
const openedTabs = new Map()
let fixtureHistoryIndex = 1
const fixtureHistory = [
  { id: 1, url: "http://127.0.0.1:17417/previous" },
  { id: 2, url: "http://127.0.0.1:17417/" },
]
const html = await readFile(join(process.cwd(), "fixtures/browser/index.html"))

function json(response, status, value) {
  response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" })
  response.end(JSON.stringify(value))
}

async function body(request) {
  let text = ""
  for await (const chunk of request) text += chunk
  return text ? JSON.parse(text) : {}
}

const server = createServer(async (request, response) => {
  if (request.method === "GET" && request.url === "/") {
    response.writeHead(200, { "content-type": "text/html; charset=utf8" })
    response.end(html)
    return
  }
  if (request.method === "GET" && request.url === "/frame.html") {
    response.writeHead(200, { "content-type": "text/html; charset=utf8", "cache-control": "no-store" })
    response.end("<!doctype html><title>Nested fixture frame</title><p id=frame-state>frame ready</p>")
    return
  }
  if (request.method === "GET" && request.url === "/json/version") {
    json(response, 200, { Browser: "ComptrolFixture/0.1", "Protocol-Version": "1.3", webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/browser/comptrol-fixture` })
    return
  }
  if (request.method === "PUT" && request.url.startsWith("/json/new?")) {
    const id = `comptrol-opened-${openedTabs.size + 1}`
    const url = decodeURIComponent(request.url.slice("/json/new?".length))
    const target = { id, type: "page", title: "opened fixture tab", url, browserContextId, revision: `fixture-${id}`, webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/page/${id}` }
    openedTabs.set(id, target)
    json(response, 200, target)
    return
  }
  if (request.method === "GET" && request.url.startsWith("/json/close/")) {
    const id = decodeURIComponent(request.url.slice("/json/close/".length))
    if (!openedTabs.delete(id)) {
      json(response, 404, { error: "target_not_found" })
      return
    }
    json(response, 200, { result: "Target is closing" })
    return
  }
  if (request.method === "GET" && request.url === "/json/list") {
    json(response, 200, [{ id: targetId, type: "page", title: "Comptrol browser fixture", url: `http://127.0.0.1:${port}/`, browserContextId, revision, webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/page/${targetId}` }, ...openedTabs.values()])
    return
  }
  if (request.method === "GET" && request.url === "/state") {
    json(response, 200, { targetId, browserContextId, revision, submissions: [...submissions.values()] })
    return
  }
  if (request.method === "GET" && request.url === "/download/fixture.txt") {
    const payload = Buffer.from("Comptrol fixture download\n")
    response.writeHead(200, {
      "content-type": "text/plain; charset=utf8",
      "content-disposition": 'attachment; filename="fixture.txt"',
      "content-length": payload.length,
      "cache-control": "no-store",
    })
    response.end(payload)
    return
  }
  if (request.method === "POST" && request.url === "/submit") {
    const target = request.headers["x-comptrol-target-id"]
    const context = request.headers["x-comptrol-browser-context"]
    const key = request.headers["x-comptrol-idempotency-key"]
    if (target !== targetId || context !== browserContextId) {
      json(response, 409, { error: "stale_reference", targetId, browserContextId, revision })
      return
    }
    if (!key) {
      json(response, 400, { error: "idempotency_required" })
      return
    }
    if (submissions.has(key)) {
      json(response, 200, { state: "replayed", result: submissions.get(key) })
      return
    }
    const payload = await body(request)
    const result = { state: "submitted", idempotency_key: key, messageLength: String(payload.message || "").length, targetId, revision }
    submissions.set(key, result)
    json(response, 200, { state: "submitted", result })
    return
  }
  json(response, 404, { error: "not_found" })
})

function websocketFrame(text) {
  const payload = Buffer.from(text)
  if (payload.length < 126) return Buffer.concat([Buffer.from([0x81, payload.length]), payload])
  if (payload.length <= 0xffff) {
    const header = Buffer.alloc(4)
    header[0] = 0x81
    header[1] = 126
    header.writeUInt16BE(payload.length, 2)
    return Buffer.concat([header, payload])
  }
  throw new Error("fixture websocket payload is too large")
}

function websocketMessage(buffer) {
  if (buffer.length < 6 || (buffer[0] & 0x80) === 0) return null
  let length = buffer[1] & 0x7f
  let offset = 2
  if (length === 126) {
    if (buffer.length < 8) return null
    length = buffer.readUInt16BE(2)
    offset = 4
  }
  if (length > 125 || buffer.length < offset + 4 + length) return null
  const mask = buffer.subarray(offset, offset + 4)
  const payload = buffer.subarray(offset + 4, offset + 4 + length)
  return Buffer.from(payload.map((value, index) => value ^ mask[index % 4])).toString()
}

server.on("upgrade", (request, socket) => {
  const browserSocket = request.url === `/devtools/browser/comptrol-fixture`
  if (!browserSocket && request.url !== `/devtools/page/${targetId}`) {
    socket.destroy()
    return
  }
  const accept = createHash("sha1").update(`${request.headers["sec-websocket-key"]}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest("base64")
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`)
  let buffer = Buffer.alloc(0)
  socket.on("data", chunk => {
    buffer = Buffer.concat([buffer, chunk])
    const text = websocketMessage(buffer)
    if (!text) return
    buffer = Buffer.alloc(0)
    const message = JSON.parse(text)
    const result = browserSocket && message.method === "Target.closeTarget"
      ? { success: openedTabs.delete(message.params.targetId) }
      : message.method === "Runtime.evaluate"
        ? { result: { type: "string", value: message.params.expression === "document.title" ? "Comptrol browser fixture" : "fixture evaluation" } }
        : message.method === "Page.getNavigationHistory"
          ? { currentIndex: fixtureHistoryIndex, entries: fixtureHistory }
          : message.method === "Page.navigateToHistoryEntry"
            ? (fixtureHistoryIndex = fixtureHistory.findIndex(entry => entry.id === message.params.entryId), {})
        : message.method === "Page.navigate"
          ? { frameId: "fixture-frame" }
          : {}
    socket.write(websocketFrame(JSON.stringify({ id: message.id, result })))
  })
})

server.listen(port, "127.0.0.1", () => {
  process.stderr.write(`Comptrol browser fixture listening on http://127.0.0.1:${port}\n`)
})
