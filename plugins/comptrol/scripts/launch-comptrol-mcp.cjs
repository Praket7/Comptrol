#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

function findLauncher() {
  const names =
    process.platform === "win32"
      ? ["comptrolling.cmd", "comptrolling.exe", "comptrolling"]
      : ["comptrolling"];
  for (const directory of (process.env.PATH || "").split(path.delimiter)) {
    for (const name of names) {
      const candidate = path.join(directory, name);
      if (fs.existsSync(candidate)) return candidate;
    }
  }
  throw new Error("The global comptrolling npm launcher was not found on PATH");
}

function findServerEntry(launcher) {
  if (process.platform === "win32" && launcher.toLowerCase().endsWith(".cmd")) {
    return path.join(
      path.dirname(launcher),
      "node_modules",
      "comptrolling",
      "bin",
      "comptrol-mcp.js",
    );
  }
  return fs.realpathSync(launcher);
}

let serverEntry;
try {
  serverEntry = findServerEntry(findLauncher());
  if (!fs.statSync(serverEntry).isFile()) {
    throw new Error("Comptrol MCP entry is not a file: " + serverEntry);
  }
} catch (error) {
  process.stderr.write("Comptrol MCP launcher: " + error.message + "\n");
  process.exit(1);
}

const child = spawn(process.execPath, [serverEntry], {
  env: process.env,
  stdio: "inherit",
});

child.on("error", (error) => {
  process.stderr.write("Comptrol MCP process failed: " + error.message + "\n");
  process.exit(1);
});
child.on("exit", (code) => process.exit(code ?? 1));
