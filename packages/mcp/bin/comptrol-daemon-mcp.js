const { spawn } = require("node:child_process");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");

const binary = process.env.COMPTROL_BIN || "comptrol";
const maxRestarts = 3;
const maxFrameBytes = 1024 * 1024;
const pending = [];
let pendingBytes = 0;
let inputBuffer = "";
let daemon;
let socket;
let socketBuffer = Buffer.alloc(0);
let restarts = 0;
let stopping = false;
let restartTimer;

function socketPath() {
  return process.env.COMPTROL_SOCKET_PATH || path.join(
    process.env.COMPTROL_STATE_DIR || path.join(os.homedir(), ".comptrol"),
    "comptrol.sock",
  );
}

function frame(line) {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    message = undefined;
  }
  const envelope = {
    version: 1,
    id: message?.id ?? null,
    method: "mcp",
    message,
  };
  if (message === undefined) {
    delete envelope.message;
    envelope.raw_message = line;
  }
  const payload = Buffer.from(JSON.stringify(envelope), "utf8");
  if (payload.length > maxFrameBytes) {
    throw new Error("MCP message exceeds the daemon frame limit");
  }
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length);
  return Buffer.concat([header, payload]);
}

function enqueue(line) {
  const chunk = frame(line);
  pending.push(chunk);
  pendingBytes += chunk.length;
  if (pendingBytes > maxFrameBytes) {
    console.error("Comptrol input exceeded the reconnect buffer limit");
    stopping = true;
    socket?.destroy();
    daemon?.kill();
    process.exitCode = 1;
    return;
  }
  flushPending();
}

function flushPending() {
  while (pending.length && socket?.writable) {
    const chunk = pending.shift();
    pendingBytes -= chunk.length;
    socket.write(chunk);
  }
}

function emitEnvelope(envelope) {
  if (envelope.event) {
    process.stdout.write(`${JSON.stringify(envelope.event)}\n`);
    return;
  }
  if (envelope.result) {
    process.stdout.write(`${JSON.stringify(envelope.result)}\n`);
    return;
  }
  process.stdout.write(`${JSON.stringify({
    jsonrpc: "2.0",
    id: envelope.id ?? null,
    error: envelope.error || { code: "daemon_error", message: "Daemon request failed" },
  })}\n`);
}

function consumeSocketData(chunk) {
  socketBuffer = Buffer.concat([socketBuffer, chunk]);
  while (socketBuffer.length >= 4) {
    const length = socketBuffer.readUInt32BE(0);
    if (length > maxFrameBytes) {
      socket?.destroy(new Error("Daemon response exceeds the frame limit"));
      return;
    }
    if (socketBuffer.length < length + 4) return;
    const payload = socketBuffer.subarray(4, length + 4);
    socketBuffer = socketBuffer.subarray(length + 4);
    try {
      emitEnvelope(JSON.parse(payload.toString("utf8")));
    } catch (error) {
      socket?.destroy(error);
      return;
    }
  }
}

function scheduleRestart(reason) {
  if (stopping || restartTimer) return;
  if (restarts >= maxRestarts) {
    console.error(`Comptrol daemon stopped after ${restarts} restarts ${reason}`);
    process.exitCode = 1;
    process.stdin.destroy();
    return;
  }
  const delay = 100 * 2 ** restarts;
  restarts += 1;
  console.error(`Comptrol daemon connection lost ${reason} reconnecting`);
  restartTimer = setTimeout(() => {
    restartTimer = undefined;
    startDaemon();
  }, delay);
}

function attachSocket(current) {
  socket = current;
  socketBuffer = Buffer.alloc(0);
  current.on("data", consumeSocketData);
  current.on("error", (error) => {
    if (socket === current) socket = undefined;
    scheduleRestart(`because ${error.message}`);
  });
  current.on("close", () => {
    if (socket === current) socket = undefined;
    if (!stopping) scheduleRestart("because the daemon socket closed");
  });
  flushPending();
}

function connectSocket(attempt = 0) {
  if (stopping) return;
  const current = net.createConnection(socketPath());
  let connected = false;
  current.once("connect", () => {
    connected = true;
    attachSocket(current);
  });
  current.once("error", (error) => {
    if (connected || stopping) return;
    current.destroy();
    if (attempt < 20) {
      setTimeout(() => connectSocket(attempt + 1), 50);
    } else {
      scheduleRestart(`because the daemon was unavailable ${error.message}`);
    }
  });
}

function startDaemon() {
  if (stopping) return;
  daemon = spawn(binary, ["daemon"], {
    stdio: ["ignore", "ignore", "inherit"],
    env: process.env,
  });
  daemon.on("error", (error) => scheduleRestart(`because daemon launch failed ${error.message}`));
  connectSocket();
}

process.stdin.on("data", (chunk) => {
  inputBuffer += chunk.toString("utf8");
  let newline;
  while ((newline = inputBuffer.indexOf("\n")) >= 0) {
    const line = inputBuffer.slice(0, newline);
    inputBuffer = inputBuffer.slice(newline + 1);
    if (line.trim()) {
      try {
        enqueue(line);
      } catch (error) {
        console.error(error.message);
        stopping = true;
        process.exitCode = 1;
        process.stdin.destroy();
      }
    }
  }
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    stopping = true;
    clearTimeout(restartTimer);
    process.stdin.destroy();
    socket?.destroy();
    daemon?.kill(signal);
  });
}

startDaemon();
