#!/usr/bin/env node
const { spawnSync } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const bridgeDir = path.join(__dirname, "..", "browser-bridge");
const installScript = path.join(bridgeDir, "install.py");
const manifest = path.join(bridgeDir, "manifest.json");

function extensionFiles(directory, relative = "") {
  const files = [];
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (
      entry.name === "__pycache__" ||
      entry.name === "native_host_manifest.json" ||
      entry.name === "native_host_config.json" ||
      entry.name.endsWith(".pyc") ||
      entry.name.endsWith(".pyo")
    ) continue;
    const source = path.join(directory, entry.name);
    const rel = path.posix.join(relative, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Refusing extension symlink: ${source}`);
    if (entry.isDirectory()) files.push(...extensionFiles(source, rel));
    else if (entry.isFile()) files.push({ source, rel, contents: fs.readFileSync(source) });
  }
  return files;
}

function prepareUnpackedExtension() {
  const stateDir = process.env.COMPTROL_STATE_DIR || path.join(os.homedir(), ".comptrol");
  const files = extensionFiles(bridgeDir).sort((left, right) => left.rel.localeCompare(right.rel));
  const digest = crypto.createHash("sha256");
  for (const file of files) digest.update(file.rel).update("\0").update(file.contents).update("\0");
  const version = JSON.parse(fs.readFileSync(manifest, "utf8")).version;
  const destination = path.join(stateDir, "browser-extensions", `${version}-${digest.digest("hex").slice(0, 16)}`);
  if (fs.existsSync(destination)) {
    const current = extensionFiles(destination).sort((left, right) => left.rel.localeCompare(right.rel));
    if (
      current.length !== files.length ||
      current.some((file, index) => file.rel !== files[index].rel || !file.contents.equals(files[index].contents))
    ) throw new Error(`Prepared extension path exists with unexpected contents: ${destination}`);
    return destination;
  }
  fs.mkdirSync(destination, { recursive: true });
  for (const file of files) {
    const target = path.join(destination, ...file.rel.split("/"));
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.copyFileSync(file.source, target);
  }
  return destination;
}

if (!fs.existsSync(installScript) || !fs.existsSync(manifest)) {
  console.error("Browser Bridge assets are missing from this package; reinstall comptrolling.");
  process.exit(1);
}

const args = process.argv.slice(2);
if (args.includes("--help")) {
  console.log("Usage: comptrol-browser-setup --print-extension-path | --extension-id <exact Chrome ID>");
  console.log("The path command prepares and prints a clean, fingerprinted unpacked-extension folder.");
  process.exit(0);
}
if (args.includes("--print-extension-path") || args.includes("--prepare-extension")) {
  try {
    console.log(prepareUnpackedExtension());
  } catch (error) {
    console.error(`Could not prepare the unpacked extension: ${error.message}`);
    process.exit(1);
  }
  process.exit(0);
}

const extensionIdIndex = args.indexOf("--extension-id");
const extensionId = extensionIdIndex >= 0 ? args[extensionIdIndex + 1] : undefined;
if (!extensionId) {
  console.error(
    [
      "Chrome requires an exact extension ID before a native messaging host can be authorized.",
      "1. Load the unpacked extension from:",
      `   ${prepareUnpackedExtension()}`,
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
