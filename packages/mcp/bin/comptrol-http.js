#!/usr/bin/env node
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

function bundledBinary() {
  const platform = `${process.platform}-${process.arch}`;
  return path.join(__dirname, "..", "native", platform, process.platform === "win32" ? "comptrol.exe" : "comptrol");
}

const binary = process.env.COMPTROL_BIN || (fs.existsSync(bundledBinary()) ? bundledBinary() : "comptrol");
const child = spawn(binary, ["serve-http", ...process.argv.slice(2)], {
  stdio: "inherit",
  env: process.env,
  shell: process.platform === "win32" && binary.toLowerCase().endsWith(".cmd"),
});

child.on("error", (error) => {
  console.error(`Comptrol HTTP server failed to start: ${error.message}`);
  process.exitCode = 1;
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exitCode = code ?? 1;
});
