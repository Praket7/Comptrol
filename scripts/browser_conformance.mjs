import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import { existsSync } from "node:fs"

const port = 17418
const fixture = spawn(process.execPath, ["scripts/browser_fixture.mjs"], { env: { ...process.env, COMPTROL_FIXTURE_PORT: String(port) }, stdio: ["ignore", "ignore", "pipe"] })
await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("fixture startup timeout")), 3000)
  fixture.stderr.once("data", () => { clearTimeout(timer); resolve() })
  fixture.once("error", reject)
})

try {
  const version = await fetch(`http://127.0.0.1:${port}/json/version`).then(response => response.json())
  const [target] = await fetch(`http://127.0.0.1:${port}/json/list`).then(response => response.json())
  assert.equal(target.id, "comptrol-fixture-page")
  const headers = { "content-type": "application/json", "x-comptrol-target-id": target.id, "x-comptrol-browser-context": target.browserContextId, "x-comptrol-idempotency-key": "same-submit" }
  const first = await fetch(`http://127.0.0.1:${port}/submit`, { method: "POST", headers, body: JSON.stringify({ message: "hello" }) }).then(response => response.json())
  const second = await fetch(`http://127.0.0.1:${port}/submit`, { method: "POST", headers, body: JSON.stringify({ message: "hello again" }) }).then(response => response.json())
  const stale = await fetch(`http://127.0.0.1:${port}/submit`, { method: "POST", headers: { ...headers, "x-comptrol-target-id": "wrong-target", "x-comptrol-idempotency-key": "new-submit" }, body: JSON.stringify({ message: "blocked" }) }).then(response => response.json())
  assert.equal(version["Protocol-Version"], "1.3")
  assert.equal(first.state, "submitted")
  assert.equal(second.state, "replayed")
  assert.equal(stale.error, "stale_reference")
  const binary = process.env.COMPTROL_BIN || "target/debug/comptrol"
  if (existsSync(binary)) {
    const comptrol = spawn(binary, ["mcp"], { env: { ...process.env, COMPTROL_CDP_ENDPOINT: `http://127.0.0.1:${port}`, COMPTROL_ALLOW_BROWSER_FIXTURE: "1", COMPTROL_ALLOW_BROWSER_CDP: "1", COMPTROL_STATE_DIR: `/tmp/comptrol-browser-${process.pid}` }, stdio: ["pipe", "pipe", "inherit"] })
    let buffer = ""
    const response = (id, message) => new Promise((resolve, reject) => {
      const onData = chunk => {
        buffer += chunk
        const line = buffer.split("\n")[0]
        if (!line) return
        buffer = buffer.slice(line.length + 1)
        try {
          const value = JSON.parse(line)
          if (value.id === id) {
            comptrol.stdout.off("data", onData)
            resolve(value)
          }
        } catch (error) {
          comptrol.stdout.off("data", onData)
          reject(error)
        }
      }
      comptrol.stdout.on("data", onData)
      comptrol.stdin.write(`${JSON.stringify(message)}\n`)
    })
    await response(1, { jsonrpc: "2.0", id: 1, method: "initialize", params: {} })
    const browser = await response(2, { jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "inspect", arguments: { kind: "browser" } } })
    const browserText = browser.result.structuredContent.targets
    assert.equal(browserText[0].id, "comptrol-fixture-page")
    const action = await response(3, { jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.fixture.submit", idempotency_key: "mcp-submit", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, message: "through MCP" } } } })
    assert.equal(action.result.structuredContent.verification, "verified")
    const cdp = await response(4, { jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.evaluate", idempotency_key: "cdp-evaluate", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, expression: "document.title" } } } })
    assert.equal(cdp.result.structuredContent.verification, "verified")
    assert.equal(cdp.result.structuredContent.data.result.value, "Comptrol browser fixture")
    comptrol.kill("SIGTERM")
  }
  console.log("browser fixture conformance passed")
} finally {
  fixture.kill("SIGTERM")
}
