#!/usr/bin/env node
const { spawn } = require("node:child_process");

const binary = process.env.COMPTROL_BIN || "comptrol";
const child = spawn(binary, ["mcp", ...process.argv.slice(2)], {
  stdio: "inherit",
  env: process.env,
});
child.on("error", (error) => {
  console.error(`Unable to start ${binary}: ${error.message}`);
  process.exitCode = 1;
});
child.on("exit", (code, signal) => {
  process.exitCode = code ?? 1;
  if (signal) console.error(`Comptrol stopped with ${signal}`);
});

