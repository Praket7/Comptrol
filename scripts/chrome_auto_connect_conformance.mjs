// Chrome permissioned auto-connect provider conformance.
//
// The V5 session broker must only attach to an endpoint that answers like a
// real Chrome DevTools endpoint (a /json/version document that carries a
// webSocketDebuggerUrl). It must never claim an owned browser for an
// existing-session attach, and it must propagate the policy environment so the
// runtime's CDP route becomes feasible without further approval.

import assert from "node:assert/strict";
import http from "node:http";

const { ensureChromeCdp } = await import("../packages/mcp/bin/chrome-cdp.js");

function serve(body) {
  return new Promise((resolve) => {
    const server = http.createServer((_request, response) => {
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify(body));
    });
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }));
  });
}

function withEnvironment(port) {
  delete process.env.COMPTROL_CDP_ENDPOINT;
  delete process.env.COMPTROL_ALLOW_BROWSER_CDP;
  process.env.COMPTROL_CHROME_CDP_PORT = String(port);
  process.env.COMPTROL_AUTO_START_CHROME_CDP = "0";
}

const real = { Browser: "Chrome/126.0", webSocketDebuggerUrl: "ws://127.0.0.1/devtools/browser/guid" };

// 1. A genuine permissioned endpoint is adopted, unowned, with policy set.
const attached = await serve(real);
try {
  withEnvironment(attached.port);
  const result = await ensureChromeCdp();
  assert.equal(result.source, "existing_permissioned");
  assert.equal(result.owned, false);
  assert.equal(result.endpoint, `http://127.0.0.1:${attached.port}`);
  assert.equal(process.env.COMPTROL_ALLOW_BROWSER_CDP, "1");
  assert.equal(process.env.COMPTROL_CDP_ENDPOINT, result.endpoint);
} finally {
  attached.server.close();
}

// 2. A lookalike without the debugger URL is rejected: Comptrol never attaches
//    to an unverified surface.
const fake = await serve({ Browser: "Chrome/126.0" });
try {
  withEnvironment(fake.port);
  const result = await ensureChromeCdp();
  assert.equal(result, undefined);
  assert.equal(process.env.COMPTROL_ALLOW_BROWSER_CDP, undefined);
} finally {
  fake.server.close();
}

// 3. With no endpoint and autostart disabled, the broker refuses instead of
//    silently starting a browser.
delete process.env.COMPTROL_CDP_ENDPOINT;
delete process.env.COMPTROL_CHROME_CDP_PORT;
process.env.COMPTROL_AUTO_START_CHROME_CDP = "0";
const refused = await ensureChromeCdp();
assert.equal(refused, undefined);

console.log("chrome auto-connect conformance passed");
