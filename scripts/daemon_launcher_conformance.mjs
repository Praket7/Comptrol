import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { once } from "node:events";

const directory = await mkdtemp(join(tmpdir(), "comptrol-daemon-launcher-"));
const fake = join(directory, "fake-comptrol.mjs");
const driver = join(directory, "driver.mjs");
const marker = join(directory, "starts");
const socket = join(directory, "comptrol.sock");
await writeFile(
  driver,
  `
import { createServer } from "node:net";
import { readFile, unlink, writeFile } from "node:fs/promises";
const socketPath = process.env.COMPTROL_SOCKET_PATH;
const marker = process.env.COMPTROL_TEST_MARKER;
const previous = await readFile(marker, "utf8").catch(() => "0");
const generation = Number(previous) + 1;
await writeFile(marker, String(generation));
await unlink(socketPath).catch(() => {});
const frame = (value) => {
  const body = Buffer.from(JSON.stringify(value));
  const header = Buffer.alloc(4);
  header.writeUInt32BE(body.length);
  return Buffer.concat([header, body]);
};
const server = createServer((connection) => {
  let buffer = Buffer.alloc(0);
  connection.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 4) {
      const length = buffer.readUInt32BE(0);
      if (buffer.length < length + 4) return;
      const value = JSON.parse(buffer.subarray(4, length + 4).toString());
      buffer = buffer.subarray(length + 4);
      connection.write(frame({ version: 1, id: value.id, result: value.message }));
      if (generation === 1) {
        setTimeout(() => { connection.destroy(); server.close(() => process.exit(7)); }, 20);
      }
    }
  });
});
server.listen(socketPath);
`,
);
const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
await writeFile(fake, `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(driver)} "$@"\n`, { mode: 0o755 });
await chmod(fake, 0o755);

const launcher = spawn(process.execPath, ["packages/mcp/bin/comptrol-mcp.js"], {
  cwd: new URL("..", import.meta.url),
  env: {
    ...process.env,
    COMPTROL_BIN: fake,
    COMPTROL_DAEMON: "1",
    COMPTROL_SOCKET_PATH: socket,
    COMPTROL_TEST_MARKER: marker,
  },
  stdio: ["pipe", "pipe", "pipe"],
});
const exited = once(launcher, "exit");
let output = "";
let errors = "";
launcher.stdout.on("data", (chunk) => { output += chunk; });
launcher.stderr.on("data", (chunk) => { errors += chunk; });
launcher.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "ping" }) + "\n");
const firstDeadline = Date.now() + 4000;
while (!output.includes('"id":1') && Date.now() < firstDeadline) {
  await new Promise((resolve) => setTimeout(resolve, 25));
}
if (!output.includes('"id":1')) {
  launcher.kill("SIGKILL");
  await exited;
  await rm(directory, { recursive: true, force: true });
  throw new Error(`daemon launcher did not answer first request ${output} ${errors}`);
}
const reconnectDeadline = Date.now() + 4000;
while (!errors.includes("reconnecting") && Date.now() < reconnectDeadline) {
  await new Promise((resolve) => setTimeout(resolve, 25));
}
launcher.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 2, method: "ping" }) + "\n");
const secondDeadline = Date.now() + 5000;
while (!output.includes('"id":2') && Date.now() < secondDeadline) {
  await new Promise((resolve) => setTimeout(resolve, 25));
}
if (!output.includes('"id":2') || !errors.includes("reconnecting")) {
  launcher.kill("SIGKILL");
  await exited;
  await rm(directory, { recursive: true, force: true });
  throw new Error(`daemon launcher did not reconnect ${output} ${errors}`);
}
launcher.kill("SIGTERM");
await exited;
const starts = await readFile(marker, "utf8");
if (Number(starts) < 2) throw new Error(`daemon was not restarted ${starts}`);
await rm(directory, { recursive: true, force: true });
console.log("daemon launcher conformance passed");
