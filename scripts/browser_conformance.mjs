import assert from "node:assert/strict"
import { spawn } from "node:child_process"

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
  console.log("browser fixture conformance passed")
} finally {
  fixture.kill("SIGTERM")
}
