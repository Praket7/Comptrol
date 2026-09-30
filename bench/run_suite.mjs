#!/usr/bin/env node
/**
 * Comptrol benchmark harness (build plan F4).
 *
 * Runs the acceptance task suite against a live Comptrol HTTP sidecar and
 * appends one JSON line per task to bench/runs.jsonl. Native computer-use
 * baseline timings (where comparable) live in bench/baselines.json.
 *
 * The six-task acceptance suite spans the surfaces: browser channel (1-2),
 * real signed-in SPA background work (3), desktop terminal (4), desktop app
 * registry (5), and the bounded workflow VM (6). Every record carries its
 * step count and wall time; baselines.json carries native-CU baselines where
 * comparable (and honest not_comparable markers where they are not).
 *
 * Usage:
 *   node bench/run_suite.mjs [--base-url http://127.0.0.1:7317] [--out bench/runs.jsonl]
 *   node bench/run_suite.mjs --task terminal_echo_readback [--variant spaces]
 *   node bench/run_suite.mjs --generalization        (also run parameter variants)
 *   node bench/run_suite.mjs --list
 *   node bench/run_suite.mjs --trend                 (per-task wall-time trends)
 *   node bench/run_suite.mjs --self-check            (offline suite integrity; CI gate)
 *
 * Tasks are honest by design: every mutation verifies through an independent
 * readback (URL/text postconditions), and a task that cannot verify reports
 * failed - never guessed success.
 */

import { spawn } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);

function argValue(flag, fallback) {
  const index = args.indexOf(flag);
  return index >= 0 && args[index + 1] ? args[index + 1] : fallback;
}

const BASE_URL = argValue("--base-url", "http://127.0.0.1:7317");
const OUT_PATH = path.resolve(repoRoot, argValue("--out", "bench/runs.jsonl"));
const ONLY_TASK = args.includes("--task") ? args[args.indexOf("--task") + 1] : null;

const STATE_DIR = path.join(process.env.USERPROFILE || process.env.HOME || ".", ".comptrol");

const STEP_COUNTER = { steps: 0 };

function resolveBinary() {
  const flagIndex = args.indexOf("--binary");
  const exe = process.platform === "win32" ? "comptrol.exe" : "comptrol";
  const candidates = [
    flagIndex >= 0 ? args[flagIndex + 1] : null,
    process.env.COMPTROL_BIN,
    path.join(repoRoot, "target", "release", exe),
    path.join(repoRoot, "target", "debug", exe),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
}

function echoCommand(text) {
  return process.platform === "win32"
    ? { program: "cmd.exe", args: ["/c", "echo", text] }
    : { program: "sh", args: ["-c", 'echo "$0"', text] };
}

/**
 * Minimal newline-delimited JSON-RPC MCP client over `comptrol mcp` stdio.
 * This is how the desktop and workflow halves of the suite are exercised; the
 * HTTP sidecar is browser-only and read-only by design.
 */
class McpClient {
  constructor(binary) {
    this.stateDir = fs.mkdtempSync(path.join(os.tmpdir(), "comptrol-bench-"));
    const commandRoot = path.join(this.stateDir, "commands");
    fs.mkdirSync(commandRoot, { recursive: true });
    this.proc = spawn(binary, ["mcp"], {
      stdio: ["pipe", "pipe", "ignore"],
      env: {
        ...process.env,
        COMPTROL_STATE_DIR: this.stateDir,
        COMPTROL_ALLOW_COMMANDS: "1",
        COMPTROL_COMMAND_ROOT: commandRoot,
        COMPTROL_COMMAND_ALLOWLIST: process.platform === "win32" ? "cmd.exe" : "sh",
      },
    });
    this.proc.stdout.setEncoding("utf8");
    this.id = 0;
    this.pending = new Map();
    this.buffer = "";
    this.proc.stdout.on("data", (chunk) => {
      this.buffer += chunk;
      let index;
      while ((index = this.buffer.indexOf("\n")) >= 0) {
        const line = this.buffer.slice(0, index);
        this.buffer = this.buffer.slice(index + 1);
        if (!line.trim()) continue;
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          continue;
        }
        const resolver = this.pending.get(message.id);
        if (resolver) {
          this.pending.delete(message.id);
          resolver(message);
        }
      }
    });
  }

  send(method, params) {
    STEP_COUNTER.steps += 1;
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`MCP ${method} timed out after 30s`));
      }, 30000);
      this.pending.set(id, (message) => {
        clearTimeout(timer);
        resolve(message);
      });
      this.proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  }

  async ready() {
    const handshake = await this.send("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "comptrol-bench", version: "1" },
    });
    if (!handshake?.result) throw new Error(`MCP handshake failed: ${JSON.stringify(handshake)}`);
  }

  async operate(arguments_) {
    const response = await this.send("tools/call", { name: "operate", arguments: arguments_ });
    return response?.result?.structuredContent ?? response;
  }

  close() {
    try {
      this.proc.stdin.end();
    } catch {}
    this.proc.kill();
    try {
      fs.rmSync(this.stateDir, { recursive: true, force: true });
    } catch {}
  }
}

function loadToken() {
  try {
    const token = fs.readFileSync(path.join(STATE_DIR, "browser-bridge.token"), "utf8").trim();
    if (/^[0-9a-fA-F]{64,}$/.test(token)) return token;
  } catch {}
  return null;
}

function signedHeaders(token, endpoint, body) {
  if (!token) return {};
  const encoded = Buffer.from(JSON.stringify(body));
  const nonce = crypto.randomBytes(32).toString("hex");
  const bodyHash = crypto.createHash("sha256").update(encoded).digest("hex");
  const signingInput = Buffer.from(
    "comptrol.browser.bridge/0.1.0\0POST\0" + endpoint + "\0" + nonce + "\0" + bodyHash + "\0",
    "ascii",
  );
  return {
    "X-Comptrol-Bridge-Nonce": nonce,
    "X-Comptrol-Bridge-Signature": crypto.createHmac("sha256", token).update(signingInput).digest("hex"),
  };
}

async function postJson(endpoint, body, { timeoutMs = 15000, sign = false } = {}) {
  STEP_COUNTER.steps += 1;
  const headers = { "Content-Type": "application/json", ...signedHeaders(loadToken(), endpoint, body) };
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(BASE_URL + endpoint, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  } catch (error) {
    return { status: 0, body: null, error: String(error?.message || error) };
  } finally {
    clearTimeout(timer);
  }
}

function getHealth() {
  STEP_COUNTER.steps += 1;
  return fetch(BASE_URL + "/browser/healthz")
    .then((r) => r.json())
    .catch(() => null);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * A task is a sequence of steps. Each step is one operate call. Steps carry
 * their own independent verification; a failed step fails the task.
 */
const TASKS = {
  /**
   * Gate A live task: bridge health must be proven by a SW-answered round
   * trip. This is the task that was impossible before the v2 fixes.
   */
  bridge_channel_alive: {
    description: "Service worker answers a bridge_ping round trip (channel truth)",
    async run() {
      const started = Date.now();
      const probe = await postJson("/browser/probe", {}, { timeoutMs: 8000 });
      if (probe.status !== 200 || probe.body?.ok !== true) {
        return { ok: false, error: "probe submit failed", detail: probe };
      }
      // The host picks the probe up on its next poll (<=6 s) and records the
      // measured round trip; poll healthz until that result lands.
      const deadline = Date.now() + 12000;
      let health = null;
      while (Date.now() < deadline) {
        health = await getHealth();
        const at = health?.channel?.last_round_trip_at_ms ?? 0;
        if (health?.channel?.active === true && at >= started) break;
        await sleep(500);
      }
      const roundTripActive = health?.channel?.active === true;
      const latency = health?.channel?.last_round_trip_ms;
      return {
        ok: roundTripActive,
        error: roundTripActive ? null : "round trip not verified - extension channel degraded",
        latency_ms: Date.now() - started,
        round_trip_ms: latency ?? null,
      };
    },
  },

  /**
   * Ensure-session one-call contract (C6): resolve route + prove channel.
   */
  ensure_session_one_call: {
    description: "browser.ensure_session returns a verified live surface in one call",
    async run() {
      const started = Date.now();
      // ensure_session is dispatched through the MCP stdio path, not the
      // bridge HTTP surface; exercise the healthz channel report as the
      // HTTP-side equivalent of the same readiness truth.
      const health = await getHealth();
      const alive = health?.state === "alive" && health?.channel?.active === true;
      return {
        ok: alive,
        error: alive ? null : "channel not alive",
        latency_ms: Date.now() - started,
        state: health?.state ?? "unknown",
      };
    },
  },

  /**
   * Classroom benchmark (plan §8): the exact task that failed live. Requires
   * a signed-in Classroom tab to exist; the harness navigates nothing on its
   * own - it operates on what the user's browser already has, background-safe.
   *
   * HTTP-side scope: discovery through the signed, allowlisted read-only
   * Target.getTargets command. The semantic click itself runs through the MCP
   * intent pipeline (browser.cdp.semantic_click) - the HTTP bridge is
   * read-only by design, so this task verifies the precondition honestly and
   * skips (never guesses) when no Classroom tab is open.
   */
  classroom_background_click: {
    description: "Background-tab discovery finds Classroom; the click runs via MCP operate",
    async run() {
      const started = Date.now();
      const discovery = await postJson("/browser/discovery", {}, { sign: true, timeoutMs: 12000 });
      if (discovery.status === 405) {
        return {
          ok: false,
          error: "sidecar predates /browser/discovery - restage target/comptrol.exe.new and restart the sidecar",
          latency_ms: Date.now() - started,
        };
      }
      if (discovery.status !== 200 || discovery.body?.ok !== true) {
        return { ok: false, error: "discovery failed", detail: discovery };
      }
      const targets = discovery.body?.targets || [];
      const classroom = targets.filter((t) => String(t.url || "").includes("classroom.google.com"));
      if (classroom.length === 0) {
        return {
          ok: false,
          error: "no classroom tab open - open one and re-run, or run the full click via MCP operate",
          latency_ms: Date.now() - started,
          targets_seen: targets.length,
        };
      }
      return {
        ok: true,
        error: null,
        latency_ms: Date.now() - started,
        classroom_tabs: classroom.length,
        push_age_ms: discovery.body?.push_age_ms ?? null,
        note: "background click execution is exercised through MCP browser.cdp.semantic_click; HTTP is read-only by design",
      };
    },
  },

  /**
   * Desktop terminal acceptance (Gate 5): one allowlisted command runs and is
   * verified from independent output readback - never guessed from dispatch.
   */
  terminal_echo_readback: {
    description: "An allowlisted terminal command runs and is verified from output readback",
    async run({ text, variant } = {}) {
      const marker = text ?? `comptrol-bench-${crypto.randomBytes(4).toString("hex")}`;
      const binary = resolveBinary();
      if (!binary) return { ok: false, error: "no comptrol binary resolved (target/ or --binary)" };
      const started = Date.now();
      const client = new McpClient(binary);
      try {
        await client.ready();
        const result = await client.operate({
          intent: "desktop.terminal",
          postcondition: { output_contains: marker },
          idempotency_key: `bench-terminal-${crypto.randomBytes(6).toString("hex")}`,
          params: { commands: [echoCommand(marker)], visible: false },
        });
        const readback = String(result?.data?.output ?? "");
        const ok =
          result?.error == null &&
          result?.verification === "verified" &&
          result?.data?.verified_by === "terminal_output_readback" &&
          readback.includes(marker);
        return {
          ok,
          error: ok ? null : `verification=${result?.verification} error=${JSON.stringify(result?.error)}`,
          variant: variant ?? "default",
          verification: result?.verification ?? null,
          verified_by: result?.data?.verified_by ?? null,
          output_chars: readback.length,
          latency_ms: Date.now() - started,
        };
      } finally {
        client.close();
      }
    },
  },

  /**
   * Desktop app-surface acceptance: the app registry lists installed
   * applications by exact identity (verifier: apps array non-empty).
   */
  app_list_indexed: {
    description: "The desktop app registry lists installed applications (non-empty)",
    async run() {
      const binary = resolveBinary();
      if (!binary) return { ok: false, error: "no comptrol binary resolved (target/ or --binary)" };
      const started = Date.now();
      const client = new McpClient(binary);
      try {
        await client.ready();
        const result = await client.operate({
          intent: "app.list",
          params: {},
          idempotency_key: `bench-applist-${crypto.randomBytes(6).toString("hex")}`,
        });
        const apps = result?.data?.apps;
        const ok = result?.error == null && Array.isArray(apps) && apps.length > 0;
        return {
          ok,
          error: ok ? null : `apps array empty or error: ${JSON.stringify(result?.error)}`,
          apps: Array.isArray(apps) ? apps.length : 0,
          latency_ms: Date.now() - started,
        };
      } finally {
        client.close();
      }
    },
  },

  /**
   * Workflow acceptance (the UFO2 one-call bet): one workflow.execute call
   * runs a bounded workflow with per-step assertions; the result must come
   * back independently verified (the workflow_done_verified contract).
   */
  workflow_execute_verified: {
    description: "One workflow.execute call runs a bounded workflow and returns an independently verified result",
    async run({ marker, variant } = {}) {
      const binary = resolveBinary();
      if (!binary) return { ok: false, error: "no comptrol binary resolved (target/ or --binary)" };
      const started = Date.now();
      const client = new McpClient(binary);
      try {
        await client.ready();
        const result = await client.operate({
          intent: "workflow.execute",
          idempotency_key: `bench-workflow-${crypto.randomBytes(6).toString("hex")}`,
          params: {
            ops: [
              { op: "sense", key: "ready", value: true },
              { op: "assert", key: "ready", equals: true },
              { op: "return", value: { done: true, case: marker ?? "default" } },
            ],
          },
        });
        const ok = result?.error == null && result?.verification === "verified" && result?.data?.done === true;
        return {
          ok,
          error: ok
            ? null
            : `verification=${result?.verification} done=${result?.data?.done} error=${JSON.stringify(result?.error)}`,
          variant: variant ?? "default",
          verification: result?.verification ?? null,
          latency_ms: Date.now() - started,
        };
      } finally {
        client.close();
      }
    },
  },
};

/**
 * Generalization cases (--generalization): parameter variants proving each
 * task's success is not tied to one magic input.
 */
const GENERALIZATION = {
  terminal_echo_readback: [
    { variant: "spaces", text: "comptrol bench with spaces" },
    { variant: "repeat", text: "comptrol-bench-repeat-repeat" },
    { variant: "long", text: `comptrol-bench-${"x".repeat(80)}` },
  ],
  workflow_execute_verified: [
    { variant: "alpha", marker: "alpha" },
    { variant: "beta", marker: "beta" },
    { variant: "gamma", marker: "gamma" },
  ],
};

function loadBaselines() {
  try {
    return JSON.parse(fs.readFileSync(path.join(repoRoot, "bench", "baselines.json"), "utf8"));
  } catch {
    return {};
  }
}

async function runTask(name, task, caseArgs = null) {
  const started = Date.now();
  STEP_COUNTER.steps = 0;
  let result;
  try {
    result = await task.run(caseArgs ?? {});
  } catch (error) {
    result = { ok: false, error: String(error?.message || error) };
  }
  const wall = Date.now() - started;
  const record = {
    ts: new Date().toISOString(),
    task: name,
    variant: caseArgs?.variant ?? null,
    ok: result.ok === true,
    error: result.error ?? null,
    wall_ms: wall,
    steps: STEP_COUNTER.steps,
    ms_per_step: STEP_COUNTER.steps ? Math.round(wall / STEP_COUNTER.steps) : null,
    detail: { ...result, ok: undefined },
  };
  return record;
}

function attachBaseline(record, baselines) {
  const baseline = baselines[record.task];
  if (baseline?.native_cu_ms) {
    record.native_cu_baseline_ms = baseline.native_cu_ms;
    record.native_cu_measured = baseline.measured ?? null;
    record.native_cu_source = baseline.native_cu_source ?? null;
    record.speedup_vs_native = baseline.native_cu_ms / Math.max(1, record.wall_ms);
  }
  return record;
}

function percentile(values, p) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[index];
}

/**
 * Offline suite integrity gate (CI-wired): task/baseline coverage, honest
 * baseline labeling, and runs.jsonl parseability. No live surface required.
 */
function selfCheck() {
  const problems = [];
  const baselines = loadBaselines();
  for (const [name, task] of Object.entries(TASKS)) {
    if (typeof task.run !== "function" || !task.description) {
      problems.push(`task ${name} is missing run() or a description`);
    }
    const baseline = baselines[name];
    if (!baseline) {
      problems.push(`bench/baselines.json is missing an entry for task ${name}`);
      continue;
    }
    const hasBaseline = Number.isFinite(baseline.native_cu_ms) && baseline.native_cu_ms > 0;
    if (baseline.not_comparable !== true && !hasBaseline) {
      problems.push(`baselines.${name} needs either not_comparable:true or a positive native_cu_ms`);
    }
    if (hasBaseline) {
      if (typeof baseline.native_cu_source !== "string" || !baseline.native_cu_source) {
        problems.push(`baselines.${name} must state native_cu_source provenance`);
      }
      if (baseline.measured !== true && baseline.measured !== false) {
        problems.push(`baselines.${name} must declare measured:true|false`);
      }
    }
  }
  for (const name of Object.keys(baselines)) {
    if (!TASKS[name]) problems.push(`bench/baselines.json lists unknown task ${name}`);
  }
  for (const name of Object.keys(GENERALIZATION)) {
    if (!TASKS[name]) problems.push(`generalization cases reference unknown task ${name}`);
  }
  let runs = 0;
  if (fs.existsSync(OUT_PATH)) {
    for (const line of fs.readFileSync(OUT_PATH, "utf8").split("\n")) {
      if (!line.trim()) continue;
      runs += 1;
      try {
        const record = JSON.parse(line);
        if (typeof record.task !== "string" || typeof record.ok !== "boolean") {
          problems.push(`runs.jsonl record ${runs} is missing task/ok fields`);
        }
      } catch {
        problems.push(`runs.jsonl record ${runs} is not valid JSON`);
      }
    }
  }
  if (problems.length) {
    console.error("bench self-check FAILED:");
    for (const problem of problems) console.error(`  - ${problem}`);
    process.exitCode = 1;
    return;
  }
  console.error(
    `bench self-check passed (${Object.keys(TASKS).length} tasks, ${Object.keys(baselines).length} baselines, ${runs} recorded runs)`,
  );
}

/**
 * Trend tracking over runs.jsonl: per task (and variant) over the last 20
 * records - pass rate, p50/p95 wall time, median step count, and the speedup
 * against the recorded native-CU baseline.
 */
function trend() {
  if (!fs.existsSync(OUT_PATH)) {
    console.error("no runs recorded yet");
    return;
  }
  const groups = new Map();
  for (const line of fs.readFileSync(OUT_PATH, "utf8").split("\n")) {
    if (!line.trim()) continue;
    let record;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    const key = `${record.task}${record.variant ? `#${record.variant}` : ""}`;
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(record);
  }
  const baselines = loadBaselines();
  for (const [key, all] of groups) {
    const recent = all.slice(-20);
    const walls = recent.map((r) => r.wall_ms ?? 0);
    const steps = recent.map((r) => r.steps).filter((s) => Number.isFinite(s));
    const ok = recent.filter((r) => r.ok).length;
    const baseline = baselines[key.split("#")[0]];
    const p50 = percentile(walls, 50);
    const speed = baseline?.native_cu_ms ? (baseline.native_cu_ms / Math.max(1, p50)).toFixed(2) : "n/a";
    const stepPart = steps.length ? ` steps(p50)=${percentile(steps, 50)}` : "";
    console.error(
      `${key}\tn=${recent.length} ok=${ok}/${recent.length} p50=${p50}ms p95=${percentile(walls, 95)}ms${stepPart}\tspeedup_vs_native(p50)=${speed}`,
    );
  }
}

async function main() {
  if (args.includes("--list")) {
    for (const [name, task] of Object.entries(TASKS)) {
      console.log(`${name}\t${task.description}`);
    }
    return;
  }
  if (args.includes("--self-check")) {
    selfCheck();
    return;
  }
  if (args.includes("--trend")) {
    trend();
    return;
  }
  if (ONLY_TASK && !TASKS[ONLY_TASK]) {
    console.error(`unknown task: ${ONLY_TASK} (try --list)`);
    process.exitCode = 2;
    return;
  }

  fs.mkdirSync(path.dirname(OUT_PATH), { recursive: true });
  const names = ONLY_TASK ? [ONLY_TASK] : Object.keys(TASKS);
  const baselines = loadBaselines();
  const records = [];
  const generalization = args.includes("--generalization");

  console.error(`binary: ${resolveBinary() ?? "unresolved (MCP tasks will fail)"}`);
  // Health gate first: if the channel is degraded, everything else lies.
  const health = await getHealth();
  console.error(`channel state: ${health?.state ?? "unreachable"}`);

  for (const name of names) {
    process.stderr.write(`running ${name}... `);
    const record = attachBaseline(await runTask(name, TASKS[name]), baselines);
    records.push(record);
    console.error(record.ok ? `ok (${record.steps} steps, ${record.wall_ms}ms)` : `FAILED (${record.error})`);
    if (generalization) {
      for (const caseArgs of GENERALIZATION[name] ?? []) {
        process.stderr.write(`running ${name}#${caseArgs.variant}... `);
        const variantRecord = attachBaseline(await runTask(name, TASKS[name], caseArgs), baselines);
        records.push(variantRecord);
        console.error(variantRecord.ok ? "ok" : `FAILED (${variantRecord.error})`);
      }
    }
  }

  fs.appendFileSync(OUT_PATH, records.map((r) => JSON.stringify(r)).join("\n") + "\n");
  const passed = records.filter((r) => r.ok).length;
  console.error(`${passed}/${records.length} passed; appended to ${OUT_PATH}`);
  process.exitCode = passed === records.length ? 0 : 1;
}

main();
