import { createServer } from "node:http"
import { readFile } from "node:fs/promises"
import { join } from "node:path"

const port = Number(process.env.COMPTROL_FIXTURE_PORT || 17417)
const targetId = "comptrol-fixture-page"
const browserContextId = "comptrol-fixture-context"
const revision = "fixture-revision-1"
const submissions = new Map()
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
  if (request.method === "GET" && request.url === "/json/version") {
    json(response, 200, { Browser: "ComptrolFixture/0.1", "Protocol-Version": "1.3", webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/browser/comptrol-fixture` })
    return
  }
  if (request.method === "GET" && request.url === "/json/list") {
    json(response, 200, [{ id: targetId, type: "page", title: "Comptrol browser fixture", url: `http://127.0.0.1:${port}/`, browserContextId, revision, webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/page/${targetId}` }])
    return
  }
  if (request.method === "GET" && request.url === "/state") {
    json(response, 200, { targetId, browserContextId, revision, submissions: [...submissions.values()] })
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
    const result = { state: "submitted", messageLength: String(payload.message || "").length, targetId, revision }
    submissions.set(key, result)
    json(response, 200, { state: "submitted", result })
    return
  }
  json(response, 404, { error: "not_found" })
})

server.listen(port, "127.0.0.1", () => {
  process.stderr.write(`Comptrol browser fixture listening on http://127.0.0.1:${port}\n`)
})
