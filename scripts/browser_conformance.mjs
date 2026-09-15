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
  const fixtureHtml = await fetch(`http://127.0.0.1:${port}/`).then(response => response.text())
  assert.match(fixtureHtml, /Ignore previous instructions/)
  assert.match(fixtureHtml, /fixture-canvas/)
  assert.match(fixtureHtml, /fixture-frame/)
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
    if (process.platform === "win32" && !binary.toLowerCase().endsWith(".exe")) {
      throw new Error(`COMPTROL_BIN points to a non-Windows binary (${binary}). Run this conformance test from WSL, or build a Windows executable and set COMPTROL_BIN to its .exe path.`)
    }
    const comptrol = spawn(binary, ["mcp"], { env: { ...process.env, COMPTROL_CDP_ENDPOINT: `http://127.0.0.1:${port}`, COMPTROL_ALLOW_BROWSER_FIXTURE: "1", COMPTROL_ALLOW_BROWSER_CDP: "1", COMPTROL_STATE_DIR: `/tmp/comptrol-browser-${process.pid}-${Date.now()}` }, stdio: ["pipe", "pipe", "inherit"] })
    let buffer = ""
    const response = (id, message) => new Promise((resolve, reject) => {
      const onData = chunk => {
        buffer += chunk
        const newline = buffer.indexOf("\n")
        if (newline < 0) return
        const line = buffer.slice(0, newline)
        buffer = buffer.slice(newline + 1)
        if (!line) return
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
    const semanticClick = await response(45, { jsonrpc: "2.0", id: 45, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.semantic_click", idempotency_key: "semantic-click-dynamic", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, locator: { role: "button", name: "Add dynamic node" }, timeout_ms: 1500 } } } })
    assert.equal(semanticClick.result.structuredContent.verification, "verified", JSON.stringify(semanticClick))
    const shadowClick = await response(48, { jsonrpc: "2.0", id: 48, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.semantic_click", idempotency_key: "semantic-click-shadow", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, locator: { role: "button", name: "Shadow action" }, timeout_ms: 1500 } } } })
    assert.equal(shadowClick.result.structuredContent.verification, "verified", JSON.stringify(shadowClick))
    const frameClick = await response(49, { jsonrpc: "2.0", id: 49, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.semantic_click", idempotency_key: "semantic-click-frame", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, locator: { role: "button", name: "Frame action" }, timeout_ms: 1500 } } } })
    assert.equal(frameClick.result.structuredContent.verification, "verified", JSON.stringify(frameClick))
    const dynamicWait = await response(46, { jsonrpc: "2.0", id: 46, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.wait_for", idempotency_key: "semantic-click-dynamic-ready", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, selector: "#dynamic-node", property: "textContent", contains: "dynamic ready" } } } })
    assert.equal(dynamicWait.result.structuredContent.verification, "verified", JSON.stringify(dynamicWait))
    const workflow = await response(47, { jsonrpc: "2.0", id: 47, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.workflow", idempotency_key: "browser-workflow", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, steps: [{ action: "click", locator: { role: "button", name: "Add dynamic node" }, timeout_ms: 1000 }] } } } })
    assert.equal(workflow.result.structuredContent.verification, "verified", JSON.stringify(workflow))
    assert.equal(workflow.result.structuredContent.data.step_count, 1)
    const navigationWorkflow = await response(50, { jsonrpc: "2.0", id: 50, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.workflow", idempotency_key: "browser-navigation-workflow", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, steps: [{ action: "navigate", url: `http://127.0.0.1:${port}/next`, url_contains: "/next", timeout_ms: 1000 }, { action: "wait_url", contains: "/next", timeout_ms: 1000 }] } } } })
    assert.equal(navigationWorkflow.result.structuredContent.verification, "verified", JSON.stringify(navigationWorkflow))
    const snapshot = await response(43, { jsonrpc: "2.0", id: 43, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.accessibility_snapshot", idempotency_key: "accessibility-snapshot", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, depth: 4 } } } })
    assert.equal(snapshot.result.structuredContent.verification, "verified")
    assert.equal(snapshot.result.structuredContent.data.snapshot.nodes[0].name.value, "Comptrol browser fixture")
    const screenshot = await response(51, { jsonrpc: "2.0", id: 51, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.screenshot", idempotency_key: "target-screenshot", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision, format: "png" } } } })
    assert.equal(screenshot.result.structuredContent.verification, "verified", JSON.stringify(screenshot))
    assert.equal(screenshot.result.structuredContent.data.evidence, "target_scoped_visual_digest")
    assert.equal(screenshot.result.structuredContent.data.encoded_bytes > 0, true)
    const closedGroup = await response(44, { jsonrpc: "2.0", id: 44, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.reopen_closed_group", idempotency_key: "closed-group-refusal" } } })
    assert.equal(closedGroup.result.structuredContent.error.code, "closed_group_unsupported")
    const historyBack = await response(41, { jsonrpc: "2.0", id: 41, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.history_back", idempotency_key: "history-back", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision } } } })
    assert.equal(historyBack.result.structuredContent.verification, "verified", JSON.stringify(historyBack))
    assert.equal(historyBack.result.structuredContent.data.direction, "back")
    const historyForward = await response(42, { jsonrpc: "2.0", id: 42, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.history_forward", idempotency_key: "history-forward", params: { target_id: target.id, browser_context_id: target.browserContextId, revision: target.revision } } } })
    assert.equal(historyForward.result.structuredContent.verification, "verified", JSON.stringify(historyForward))
    assert.equal(historyForward.result.structuredContent.data.direction, "forward")
    const opened = await response(5, { jsonrpc: "2.0", id: 5, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.open_tab", idempotency_key: "open-visible-tab", params: { url: `http://127.0.0.1:${port}/`, browser_context_id: target.browserContextId } } } })
    assert.equal(opened.result.structuredContent.verification, "verified")
    assert.equal(opened.result.structuredContent.data.visibility, "foreground")
    assert.equal(opened.result.structuredContent.data.mouse, "untouched")
    assert.equal(opened.result.structuredContent.data.clipboard, "untouched")
    const openedTarget = opened.result.structuredContent.data.target
    const closed = await response(6, { jsonrpc: "2.0", id: 6, method: "tools/call", params: { name: "operate", arguments: { intent: "browser.cdp.close_tab", idempotency_key: "close-visible-tab", params: { target_id: openedTarget.id, browser_context_id: openedTarget.browser_context_id, revision: openedTarget.revision } } } })
    assert.equal(closed.result.structuredContent.verification, "verified")
    assert.equal(closed.result.structuredContent.data.closed, true)
    assert.equal(closed.result.structuredContent.data.mouse, "untouched")
    assert.equal(closed.result.structuredContent.data.clipboard, "untouched")
    const metrics = await fetch(`http://127.0.0.1:${port}/metrics`).then(response => response.json())
    assert.equal(metrics.pageWebsocketConnections, 1, JSON.stringify(metrics))
    assert.equal(metrics.browserWebsocketConnections, 1, JSON.stringify(metrics))
    comptrol.kill("SIGTERM")
  }
  console.log("browser fixture conformance passed")
} finally {
  fixture.kill("SIGTERM")
}
