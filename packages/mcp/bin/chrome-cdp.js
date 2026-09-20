const { spawn } = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");

let ownedChrome;

function chromeCandidates() {
  if (process.platform === "win32") {
    return [
      path.join(process.env.PROGRAMFILES || "", "Google", "Chrome", "Application", "chrome.exe"),
      path.join(process.env["PROGRAMFILES(X86)"] || "", "Google", "Chrome", "Application", "chrome.exe"),
      path.join(process.env.LOCALAPPDATA || "", "Google", "Chrome", "Application", "chrome.exe"),
    ];
  }
  if (process.platform === "darwin") return ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"];
  return ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"];
}

function findChrome() {
  for (const candidate of chromeCandidates()) {
    if (path.isAbsolute(candidate) && fs.existsSync(candidate)) return candidate;
    if (!path.isAbsolute(candidate)) return candidate;
  }
  return undefined;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
}

function getJson(endpoint) {
  return new Promise((resolve, reject) => {
    const request = http.get(`${endpoint}/json/version`, { timeout: 400 }, (response) => {
      let body = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => { body += chunk; });
      response.on("end", () => {
        if (response.statusCode !== 200) return reject(new Error(`HTTP ${response.statusCode}`));
        try { resolve(JSON.parse(body)); } catch (error) { reject(error); }
      });
    });
    request.once("timeout", () => request.destroy(new Error("timeout")));
    request.once("error", reject);
  });
}

async function waitForEndpoint(endpoint, attempts = 80) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      const version = await getJson(endpoint);
      if (version.webSocketDebuggerUrl) return version;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return undefined;
}

function defaultProfile() {
  const root = process.env.COMPTROL_STATE_DIR || path.join(os.homedir(), ".comptrol");
  return path.join(root, "chrome-cdp-profile");
}

// Chrome 136+ deliberately ignores --remote-debugging-port/--pipe for the
// default user-data directory, and profile copying is forbidden. The
// permissioned route is the only path into the user's signed-in session:
// the user enables Remote Debugging at chrome://inspect/#remote-debugging,
// Chrome shows its native permission dialog, and the user clicks Allow.
// Everything Comptrol may do automatically is: detect an already-open
// endpoint, or start Chrome with a DEDICATED (non-default) profile.
async function detectExistingCdpEndpoint() {
  const explicit = process.env.COMPTROL_CDP_ENDPOINT;
  if (explicit) {
    const endpoint = explicit.replace(/\/$/, "");
    if (await waitForEndpoint(endpoint, 2)) return { endpoint, owned: false, source: "configured" };
    console.error(`Comptrol CDP endpoint is configured but unreachable: ${endpoint}`);
    return undefined;
  }
  // A permissioned existing-session Chrome or any user-launched
  // debug-enabled Chrome exposes /json/version on its loopback port.
  for (const port of [process.env.COMPTROL_CHROME_CDP_PORT, "9222"].filter(Boolean)) {
    const endpoint = `http://127.0.0.1:${port}`;
    if (await waitForEndpoint(endpoint, 1)) {
      return { endpoint, owned: false, source: "existing_permissioned" };
    }
  }
  return undefined;
}

async function ensureChromeCdp() {
  const existing = await detectExistingCdpEndpoint();
  if (existing) {
    process.env.COMPTROL_CDP_ENDPOINT = existing.endpoint;
    process.env.COMPTROL_ALLOW_BROWSER_CDP = "1";
    process.env.COMPTROL_AUTO_START_CHROME_CDP = "0";
    if (existing.source === "existing_permissioned") {
      console.error(`Comptrol attached to an existing permissioned Chrome CDP endpoint at ${existing.endpoint}`);
    }
    return existing;
  }
  if (process.env.COMPTROL_AUTO_START_CHROME_CDP === "0") return undefined;

  const chrome = findChrome();
  if (!chrome) {
    console.error("Comptrol could not find Chrome; set COMPTROL_CDP_ENDPOINT or install Google Chrome");
    return undefined;
  }
  const port = await freePort();
  const endpoint = `http://127.0.0.1:${port}`;
  const profile = path.resolve(process.env.COMPTROL_CHROME_PROFILE || defaultProfile());
  fs.mkdirSync(profile, { recursive: true });
  const args = [
    "--remote-debugging-address=127.0.0.1",
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-session-crashed-bubble",
    process.env.COMPTROL_CHROME_START_URL || "about:blank",
  ];
  ownedChrome = spawn(chrome, args, { stdio: "ignore", windowsHide: true });
  const version = await waitForEndpoint(endpoint);
  if (!version) {
    ownedChrome.kill();
    ownedChrome = undefined;
    console.error(`Comptrol Chrome started but did not expose CDP at ${endpoint}. Check ${profile}`);
    return undefined;
  }
  process.env.COMPTROL_CDP_ENDPOINT = endpoint;
  process.env.COMPTROL_ALLOW_BROWSER_CDP = "1";
  process.env.COMPTROL_AUTO_START_CHROME_CDP = "0";
  console.error(`Comptrol Chrome CDP ready at ${endpoint} using isolated profile ${profile}`);
  return { endpoint, owned: true, version, source: "dedicated_profile" };
}

function closeOwnedChrome() {
  if (ownedChrome && !ownedChrome.killed) ownedChrome.kill();
  ownedChrome = undefined;
}

module.exports = { ensureChromeCdp, closeOwnedChrome };
