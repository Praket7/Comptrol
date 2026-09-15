#!/usr/bin/env node
if (process.env.COMPTROL_DAEMON === "1") {
  require("./comptrol-daemon-mcp.js");
} else {
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

function bundledBinary() {
  const platform = `${process.platform}-${process.arch}`;
  return path.join(__dirname, "..", "native", platform, process.platform === "win32" ? "comptrol.exe" : "comptrol");
}

const binary = process.env.COMPTROL_BIN || (fs.existsSync(bundledBinary()) ? bundledBinary() : undefined);
if (!binary) {
  console.error(`Comptrol native binary is missing for ${process.platform}-${process.arch}; reinstall the package or set COMPTROL_BIN`);
  process.exitCode = 1;
  return;
}
const args = ["mcp", ...process.argv.slice(2)];
const maxRestarts = 3;
const pending = [];
let pendingBytes = 0;
let child;
let restarts = 0;
let stopping = false;
let restartTimer;

function flushPending() {
  while (pending.length && child?.stdin.writable) {
    const chunk = pending.shift();
    pendingBytes -= chunk.length;
    child.stdin.write(chunk);
  }
}

function scheduleRestart(reason) {
  if (stopping || restarts >= maxRestarts) {
    console.error(`Comptrol stopped after ${restarts} restarts ${reason}`);
    process.exitCode = 1;
    process.stdin.destroy();
    return;
  }
  const delay = 100 * 2 ** restarts;
  restarts += 1;
  console.error(`Comptrol connection lost ${reason} reconnecting`);
  restartTimer = setTimeout(start, delay);
}

function start() {
  const current = spawn(binary, args, {
    stdio: ["pipe", "pipe", "inherit"],
    env: process.env,
    shell: process.platform === "win32" && binary.toLowerCase().endsWith(".cmd"),
  });
  child = current;
  let handled = false;
  current.stdout.pipe(process.stdout);
  current.on("error", (error) => {
    if (handled) return;
    handled = true;
    if (child === current) child = undefined;
    scheduleRestart(`because ${error.message}`);
  });
  current.stdin.on("error", (error) => {
    if (handled || stopping) return;
    handled = true;
    if (child === current) child = undefined;
    scheduleRestart(`because the child input closed ${error.message}`);
  });
  current.on("exit", (code, signal) => {
    if (handled) return;
    handled = true;
    if (child === current) child = undefined;
    if (stopping) {
      process.exitCode = code ?? 1;
      if (signal) console.error(`Comptrol stopped with ${signal}`);
      return;
    }
    scheduleRestart(`with ${signal || `exit ${code ?? 1}`}`);
  });
  flushPending();
}

process.stdin.on("data", (chunk) => {
  if (child?.stdin.writable) {
    child.stdin.write(chunk);
    return;
  }
  pending.push(chunk);
  pendingBytes += chunk.length;
  if (pendingBytes > 1024 * 1024) {
    console.error("Comptrol input exceeded the reconnect buffer limit");
    process.exitCode = 1;
    stopping = true;
    child?.kill();
  }
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    stopping = true;
    clearTimeout(restartTimer);
    process.stdin.destroy();
    child?.kill(signal);
  });
}

start();
}
