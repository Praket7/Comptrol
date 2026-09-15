import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { once } from "node:events";

const directory = await mkdtemp(join(tmpdir(), "comptrol-launcher-"));
const fake = join(directory, "fake-comptrol.mjs");
const driver = join(directory, "driver.mjs");
const marker = join(directory, "starts");
await writeFile(
  driver,
  `
import { appendFile, readFile } from "node:fs/promises";
import process from "node:process";
const marker = process.env.COMPTROL_TEST_MARKER;
let input = "";
process.stdin.on("data", async (chunk) => {
  input += chunk;
  if (!input.includes("\\n")) return;
  const previous = await readFile(marker, "utf8").catch(() => "0");
  if (previous === "0") {
    await appendFile(marker, "1");
    process.exit(7);
  }
  process.stdout.write(JSON.stringify({ ok: true }) + "\\n");
});
`,
);
const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
const fakeCommand = process.platform === "win32"
  ? `@echo off\r\n"${process.execPath}" "${driver}" %*\r\n`
  : `#!/bin/sh\nexec ${quote(process.execPath)} ${quote(driver)} "$@"\n`;
const fakePath = process.platform === "win32" ? join(directory, "fake-comptrol.cmd") : fake;
await writeFile(fakePath, fakeCommand, {
  mode: 0o755,
});
await chmod(fakePath, 0o755);

const launcher = spawn(process.execPath, ["packages/mcp/bin/comptrol-mcp.js"], {
  cwd: new URL("..", import.meta.url),
  env: { ...process.env, COMPTROL_BIN: fakePath, COMPTROL_TEST_MARKER: marker },
  stdio: ["pipe", "pipe", "pipe"],
});
const exited = once(launcher, "exit");
let output = "";
let errors = "";
launcher.stdout.on("data", (chunk) => {
  output += chunk;
});
launcher.stderr.on("data", (chunk) => {
  errors += chunk;
});
launcher.stdin.write("first\n");
await new Promise((resolve) => setTimeout(resolve, 500));
launcher.stdin.write("second\n");
const deadline = Date.now() + 3000;
while (!output.includes('"ok":true') && Date.now() < deadline) {
  await new Promise((resolve) => setTimeout(resolve, 50));
}
if (!output.includes('"ok":true')) {
  launcher.kill("SIGKILL");
  await exited;
  await rm(directory, { recursive: true, force: true });
  throw new Error(`launcher did not reconnect ${output} ${errors}`);
}
if (!errors.includes("reconnecting")) {
  launcher.kill("SIGKILL");
  await exited;
  await rm(directory, { recursive: true, force: true });
  throw new Error(`launcher did not report reconnect ${errors}`);
}
launcher.kill("SIGTERM");
await exited;
await rm(directory, { recursive: true, force: true });
console.log("launcher restart conformance passed");
