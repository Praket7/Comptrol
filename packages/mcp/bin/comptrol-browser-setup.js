#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const bridgeDir = path.join(__dirname, "..", "browser-bridge");
const installScript = path.join(bridgeDir, "install.py");
const manifest = path.join(bridgeDir, "manifest.json");

if (!fs.existsSync(installScript) || !fs.existsSync(manifest)) {
  console.error("Browser Bridge assets are missing from this package; reinstall comptrolling.");
  process.exit(1);
}

const args = process.argv.slice(2);
if (args.includes("--print-extension-path")) {
  console.log(bridgeDir);
  process.exit(0);
}

const extensionIdIndex = args.indexOf("--extension-id");
const extensionId = extensionIdIndex >= 0 ? args[extensionIdIndex + 1] : undefined;
if (!extensionId) {
  console.error(
    [
      "Chrome requires an exact extension ID before a native messaging host can be authorized.",
      "1. Load the unpacked extension from:",
      `   ${bridgeDir}`,
      "2. Copy its extension ID from chrome://extensions.",
      "3. Run: npx comptrol-browser-setup --extension-id <ID>",
      "",
      "Use --print-extension-path to print only the extension folder."
    ].join("\n")
  );
  process.exit(2);
}

const pythonCandidates = process.platform === "win32"
  ? [["py", ["-3"]], ["python", []], ["python3", []]]
  : [["python3", []], ["python", []]];

let result;
for (const [python, prefix] of pythonCandidates) {
  result = spawnSync(
    python,
    [...prefix, installScript, "--extension-id", extensionId],
    { stdio: "inherit", env: process.env }
  );
  if (!result.error) break;
}

if (!result || result.error) {
  console.error("Python 3 is required to register the Browser Bridge native host.");
  process.exit(1);
}
if ((result.status ?? 1) === 0) {
  const stateDir = process.env.COMPTROL_STATE_DIR || path.join(os.homedir(), ".comptrol");
  fs.mkdirSync(stateDir, { recursive: true });
  fs.writeFileSync(
    path.join(stateDir, "browser-bridge.enabled"),
    JSON.stringify({ enabled: true, extensionId, configuredAt: new Date().toISOString() }) + "\n",
    { mode: 0o600 }
  );
  console.error("Browser Bridge is enabled for Comptrol. Normal comptrolling startup will manage the loopback bridge sidecar.");
}
process.exit(result.status ?? 1);
