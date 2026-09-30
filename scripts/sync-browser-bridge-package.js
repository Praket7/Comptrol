#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const repoRoot = path.resolve(__dirname, "..");
const sourceRoot = path.join(repoRoot, "extensions", "comptrol-browser-bridge");
const packageRoot = path.join(repoRoot, "packages", "mcp", "browser-bridge");
let copiedFiles = 0;

function copyTree(source, destination) {
  for (const entry of fs.readdirSync(source, { withFileTypes: true })) {
    if (
      entry.name === "__pycache__" ||
      entry.name === "native_host.bat" ||
      entry.name === "native_host_config.json" ||
      entry.name.endsWith(".pyc") ||
      entry.name.endsWith(".pyo")
    ) {
      continue;
    }
    const sourcePath = path.join(source, entry.name);
    const destinationPath = path.join(destination, entry.name);
    if (entry.isSymbolicLink()) {
      throw new Error(`Browser Bridge packaging refuses symlink: ${sourcePath}`);
    }
    if (entry.isDirectory()) {
      fs.mkdirSync(destinationPath, { recursive: true });
      copyTree(sourcePath, destinationPath);
    } else if (entry.isFile()) {
      fs.copyFileSync(sourcePath, destinationPath);
      copiedFiles += 1;
    }
  }
}

if (!fs.existsSync(path.join(sourceRoot, "manifest.json"))) {
  throw new Error(`Browser Bridge source manifest is missing: ${sourceRoot}`);
}
fs.mkdirSync(packageRoot, { recursive: true });
copyTree(sourceRoot, packageRoot);
fs.writeFileSync(
  path.join(packageRoot, ".npmignore"),
  "native_host.bat\nnative_host_config.json\n",
  "utf8",
);
console.error(`Synced ${copiedFiles} clean Browser Bridge source files into the npm package.`);
