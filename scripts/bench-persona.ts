#!/usr/bin/env bun

// Reproducible process-tree A/B harness for Persona's Electron reference and
// the Pocket-native implementation. The two targets run sequentially so they
// never compete for CPU/GPU/memory during sampling.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  realpathSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import {
  arch,
  cpus,
  homedir,
  platform,
  release,
  tmpdir,
  totalmem,
} from "node:os";
import { createServer, type AddressInfo, type Server } from "node:net";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export type TargetName = "reference" | "pocket";
type RunStatus = "running" | "ok" | "failed";

interface Options {
  referenceBin: string;
  referenceRoot: string;
  pocketBin: string;
  library: string;
  bundle: string;
  maxFps: number | null;
  maxTextureDim: number | null;
  settleSeconds: number;
  sampleCount: number;
  intervalSeconds: number;
  outPath: string;
}

interface LaunchSpec {
  target: TargetName;
  argv: string[];
  cwd: string;
  envOverrides: Record<string, string>;
  bridgePort: number;
  healthUrl: string;
  cdpPort?: number;
  cdpListUrl?: string;
}

type JsonObject = Record<string, unknown>;

interface HealthCapture {
  captured_at: string;
  response_time_ms: number;
  http_status: number;
  body: JsonObject;
}

interface ReferenceCanvasReceipt {
  ready_state: string;
  viewport: [number, number, number];
  canvas: {
    client: [number, number];
    backing: [number, number];
  };
  webgl: {
    version: string;
    renderer: string;
    vendor: string;
  };
}

interface ReferenceCdpReceipt {
  ready_at: string;
  target: {
    id: string;
    title: string;
    url: string;
  };
  canvas: ReferenceCanvasReceipt;
}

interface ReferenceFrameSample {
  requested_duration_ms: number;
  elapsed_ms: number;
  frames: number;
  fps: number;
  mean_interval_ms: number;
  p50_interval_ms: number;
  p95_interval_ms: number;
  p99_interval_ms: number;
  max_interval_ms: number;
  over_20ms: number;
}

interface ReferenceFrameReceipt {
  captured_at: string;
  frame_sample: ReferenceFrameSample;
  task_duration_delta_seconds: number;
  main_thread_cpu_percent: number;
  performance_deltas_seconds: {
    TaskDuration: number;
    ScriptDuration: number;
    LayoutDuration: number;
    RecalcStyleDuration: number;
    V8CompileDuration: number;
  };
}

interface ProcessRow {
  pid: number;
  ppid: number;
  state: string;
  cpu_time: string;
  cpu_time_seconds: number;
  ps_cpu_percent: number;
  rss_kib: number;
  command: string;
}

interface TreeSnapshot {
  captured_at: string;
  root_pid: number;
  cumulative_cpu_time_seconds: number;
  ps_cpu_percent_sum_diagnostic: number;
  rss_kib: number;
  process_count: number;
  processes: ProcessRow[];
}

interface TimedTreeSnapshot {
  clock_ms: number;
  snapshot: TreeSnapshot;
}

interface IntervalProcessRow extends ProcessRow {
  interval_cpu_time_seconds: number;
  interval_cpu_percent: number;
  interval_identity: "continued" | "new-or-restarted";
}

interface ResourceSample {
  index: number;
  interval_started_at: string;
  captured_at: string;
  elapsed_since_baseline_seconds: number;
  interval_seconds: number;
  root_pid: number;
  interval_cpu_time_seconds: number;
  interval_cpu_percent: number;
  cumulative_cpu_time_seconds: number;
  ps_cpu_percent_sum_diagnostic: number;
  rss_kib: number;
  process_count: number;
  processes: IntervalProcessRow[];
  disappeared_processes: ProcessRow[];
  health: HealthCapture;
}

interface SummaryStats {
  n: number;
  mean: number;
  min: number;
  median: number;
  p95: number;
  max: number;
}

interface RunSummary {
  interval_cpu_percent: SummaryStats;
  rss_kib: SummaryStats;
  process_count: SummaryStats;
  render_fps: SummaryStats | null;
}

interface RunResult {
  status: RunStatus;
  root_pid: number;
  launched_at: string;
  ready_at: string | null;
  readiness_health: HealthCapture | null;
  reference_cdp: ReferenceCdpReceipt | null;
  frame_receipt: ReferenceFrameReceipt | null;
  settled_at: string | null;
  sampling_completed_at: string | null;
  terminated_at: string | null;
  baseline: TreeSnapshot | null;
  samples: ResourceSample[];
  summary: RunSummary | null;
  error?: string;
}

interface Report {
  schema_version: 4;
  benchmark: "persona-reference-vs-pocket";
  status: "running" | "ok" | "failed" | "interrupted";
  started_at: string;
  completed_at: string | null;
  elapsed_seconds: number | null;
  configuration: {
    settle_seconds: number;
    sample_count: number;
    interval_seconds: number;
    max_fps: number | null;
    max_texture_dim: number | null;
    readiness_timeout_seconds: number;
    cdp_timeout_seconds: number;
    output: string;
  };
  methodology: {
    primary_cpu_metric: "interval_cpu_percent";
    cpu_time_source: "ps cumulative time/cputime per process";
    interval_cpu_formula: "sum(process CPU-time deltas) / wall interval * 100";
    cpu_percent_scale: "100 percent equals one logical core";
    ps_cpu_percent_role: "diagnostic-only";
    occlusion_policy: string;
    readiness_policy: string;
    sample_health_policy: string;
    reference_frame_policy: string;
    pocket_frame_policy: string;
    exited_process_caveat: string;
  };
  inputs: {
    reference_root: string;
    library: string;
    bundle: string;
  };
  machine: ReturnType<typeof machineFacts>;
  commands: Partial<Record<TargetName, LaunchSpec>>;
  runs: Partial<Record<TargetName, RunResult>>;
  comparison: ReturnType<typeof compareRuns> | null;
  error?: string;
}

type SpawnedProcess = ReturnType<typeof Bun.spawn>;

const REPO_ROOT = fileURLToPath(new URL("..", import.meta.url));
const DEFAULT_SETTLE_SECONDS = 15;
const DEFAULT_SAMPLE_COUNT = 13;
const DEFAULT_INTERVAL_SECONDS = 5;
const READINESS_TIMEOUT_SECONDS = 30;
const CDP_TIMEOUT_SECONDS = 30;
const HEALTH_POLL_INTERVAL_MS = 250;
const HEALTH_REQUEST_TIMEOUT_MS = 1_000;
const CDP_POLL_INTERVAL_MS = 250;
const CDP_COMMAND_TIMEOUT_MS = 10_000;
const REFERENCE_VIEWPORT = {
  width: 430,
  height: 680,
  deviceScaleFactor: 1.5,
} as const;
const REFERENCE_CANVAS_BACKING = [645, 1020] as const;
const TERM_GRACE_MS = 4_000;
const KILL_GRACE_MS = 2_000;
const POLL_MS = 100;

let activeRun: { target: TargetName; child: SpawnedProcess } | null = null;
const terminationPromises = new Map<number, Promise<void>>();
let persistInterruptedReport: ((signal: string) => void) | null = null;
let signalShutdown: Promise<void> | null = null;
let signalHandlersInstalled = false;

if (import.meta.main) {
  process.exitCode = await runPersonaBenchmark(Bun.argv.slice(2));
}

export async function runPersonaBenchmark(argv: string[]): Promise<number> {
  installSignalHandlers();
  return main(argv);
}

async function main(argv: string[]): Promise<number> {
  let options: Options;
  try {
    options = parseArgs(argv);
    validateInputs(options);
    prepareOutput(options.outPath);
  } catch (error) {
    console.error(errorMessage(error));
    console.error("");
    console.error(usage());
    return 2;
  }

  const startedAt = new Date();
  const startedClock = performance.now();
  const report: Report = {
    schema_version: 4,
    benchmark: "persona-reference-vs-pocket",
    status: "running",
    started_at: startedAt.toISOString(),
    completed_at: null,
    elapsed_seconds: null,
    configuration: {
      settle_seconds: options.settleSeconds,
      sample_count: options.sampleCount,
      interval_seconds: options.intervalSeconds,
      max_fps: options.maxFps,
      max_texture_dim: options.maxTextureDim,
      readiness_timeout_seconds: READINESS_TIMEOUT_SECONDS,
      cdp_timeout_seconds: CDP_TIMEOUT_SECONDS,
      output: options.outPath,
    },
    methodology: {
      primary_cpu_metric: "interval_cpu_percent",
      cpu_time_source: "ps cumulative time/cputime per process",
      interval_cpu_formula:
        "sum(process CPU-time deltas) / wall interval * 100",
      cpu_percent_scale: "100 percent equals one logical core",
      ps_cpu_percent_role: "diagnostic-only",
      occlusion_policy:
        "both targets use normal compositor visibility; keep each benchmark window visible and uncovered",
      readiness_policy:
        "each target must return HTTP 200 JSON with ok:true from its isolated loopback /health endpoint before settling",
      sample_health_policy:
        "every resource sample includes a contemporaneous successful /health response",
      reference_frame_policy:
        "Electron must expose a non-settings CDP page with a ready 430x680 DPR 1.5 WebGL canvas backed by 645x1020 pixels; one lightweight rAF promise spans the resource sampling window",
      pocket_frame_policy:
        "every Pocket health receipt must report modelConfigured=true, windowVisible=true, and renderFps>1",
      exited_process_caveat:
        "CPU accrued after the previous snapshot by a process that exits before the next snapshot is not observable",
    },
    inputs: {
      reference_root: options.referenceRoot,
      library: options.library,
      bundle: options.bundle,
    },
    machine: machineFacts(),
    commands: {},
    runs: {},
    comparison: null,
  };

  let referenceBridgePort: number;
  let pocketBridgePort: number;
  let referenceCdpPort: number;
  try {
    [referenceBridgePort, pocketBridgePort, referenceCdpPort] =
      await allocateLoopbackPorts(3);
  } catch (error) {
    report.status = "failed";
    report.error = `unable to allocate loopback ports: ${errorMessage(error)}`;
    finishReportClock(report, startedClock);
    writeReport(options.outPath, report);
    console.error(`bench-persona: ${report.error}`);
    return 1;
  }

  const userDataDir = mkdtempSync(
    join(tmpdir(), "pocket-character-persona-bench-"),
  );
  persistInterruptedReport = (signal) => {
    report.status = "interrupted";
    report.error = `interrupted by ${signal}`;
    finishReportClock(report, startedClock);
    try {
      writeReport(options.outPath, report);
    } finally {
      rmSync(userDataDir, { recursive: true, force: true });
    }
  };

  const referenceSpec: LaunchSpec = {
    target: "reference",
    argv: [
      options.referenceBin,
      options.referenceRoot,
      `--user-data-dir=${userDataDir}`,
      `--remote-debugging-port=${referenceCdpPort}`,
    ],
    cwd: options.referenceRoot,
    envOverrides: {
      PERSONA_BRIDGE_PORT: String(referenceBridgePort),
      // The benchmark drives the shared event contract directly. Do not let a
      // concurrently running voice app add an optional native capture helper
      // to only the Electron process tree.
      PERSONA_TARGET_PROCESS_PATTERN: "a^",
    },
    bridgePort: referenceBridgePort,
    healthUrl: `http://127.0.0.1:${referenceBridgePort}/health`,
    cdpPort: referenceCdpPort,
    cdpListUrl: `http://127.0.0.1:${referenceCdpPort}/json/list`,
  };
  const pocketSpec: LaunchSpec = {
    target: "pocket",
    argv: [
      options.pocketBin,
      "--library",
      options.library,
      "--bundle",
      options.bundle,
      ...(options.maxFps == null
        ? []
        : ["--max-fps", String(options.maxFps)]),
      ...(options.maxTextureDim == null
        ? []
        : ["--max-texture-dim", String(options.maxTextureDim)]),
      "--bridge-port",
      String(pocketBridgePort),
    ],
    cwd: process.cwd(),
    envOverrides: {},
    bridgePort: pocketBridgePort,
    healthUrl: `http://127.0.0.1:${pocketBridgePort}/health`,
  };
  report.commands.reference = referenceSpec;
  report.commands.pocket = pocketSpec;

  let exitCode = 0;
  try {
    await benchmarkTarget(referenceSpec, options, (run) => {
      report.runs.reference = run;
    });

    // The reference must be fully gone before the Pocket target starts.
    if (options.outPath !== "-") writeReport(options.outPath, report);

    await benchmarkTarget(pocketSpec, options, (run) => {
      report.runs.pocket = run;
    });

    const reference = report.runs.reference;
    const pocket = report.runs.pocket;
    if (!reference?.summary || !pocket?.summary) {
      throw new Error("both benchmark runs must have summaries");
    }
    if (reference.frame_receipt == null || pocket.summary.render_fps == null) {
      throw new Error("frame receipts are incomplete");
    }
    report.comparison = compareRuns(reference, pocket);
    report.status = "ok";
  } catch (error) {
    report.status = "failed";
    report.error = errorMessage(error);
    console.error(`bench-persona: ${report.error}`);
    exitCode = 1;
  } finally {
    persistInterruptedReport = null;
    rmSync(userDataDir, { recursive: true, force: true });
    finishReportClock(report, startedClock);
    writeReport(options.outPath, report);
  }

  if (options.outPath === "-") {
    console.error("bench-persona: JSON written to stdout");
  } else {
    console.error(`bench-persona: report ${options.outPath}`);
  }
  return exitCode;
}

export async function allocateLoopbackPorts(count: number): Promise<number[]> {
  if (!Number.isInteger(count) || count < 1) {
    throw new Error(`invalid loopback port count ${count}`);
  }
  const servers: Server[] = Array.from({ length: count }, () => createServer());
  const ports: number[] = [];
  try {
    for (const server of servers) {
      await new Promise<void>((resolveListen, rejectListen) => {
        const onError = (error: Error) => rejectListen(error);
        server.once("error", onError);
        server.listen(
          {
            host: "127.0.0.1",
            port: 0,
            exclusive: true,
          },
          () => {
            server.off("error", onError);
            resolveListen();
          },
        );
      });
      const address = server.address();
      if (address == null || typeof address === "string") {
        throw new Error("loopback reservation returned no numeric address");
      }
      ports.push((address as AddressInfo).port);
    }
  } finally {
    await Promise.all(
      servers.map(
        (server) =>
          new Promise<void>((resolveClose, rejectClose) => {
            if (!server.listening) {
              resolveClose();
              return;
            }
            server.close((error) => {
              if (error) rejectClose(error);
              else resolveClose();
            });
          }),
      ),
    );
  }
  if (ports.length !== count || new Set(ports).size !== count) {
    throw new Error(
      `expected ${count} distinct loopback ports, got ${ports.join(",")}`,
    );
  }
  return ports;
}

async function waitForReadyHealth(
  spec: LaunchSpec,
  child: SpawnedProcess,
): Promise<HealthCapture> {
  const deadline =
    performance.now() + READINESS_TIMEOUT_SECONDS * 1_000;
  let lastFailure = "health endpoint has not responded";

  while (performance.now() < deadline) {
    const remainingMs = deadline - performance.now();
    const result = await probeHealth(
      spec,
      child,
      Math.max(1, Math.min(HEALTH_REQUEST_TIMEOUT_MS, remainingMs)),
    );
    if (result.ok) {
      const validationFailure = healthValidationFailure(spec, result.capture);
      if (validationFailure == null) return result.capture;
      lastFailure = validationFailure;
    } else {
      lastFailure = result.failure;
    }

    const waitMs = Math.min(
      HEALTH_POLL_INTERVAL_MS,
      Math.max(0, deadline - performance.now()),
    );
    if (waitMs > 0) {
      await delayWhileRunning(
        child,
        waitMs,
        `${spec.target} exited before readiness`,
      );
    }
  }

  throw new Error(
    `${spec.target} readiness timed out after ${READINESS_TIMEOUT_SECONDS}s: ${lastFailure}`,
  );
}

async function captureRequiredHealth(
  spec: LaunchSpec,
  child: SpawnedProcess,
  context: string,
): Promise<HealthCapture> {
  const result = await probeHealth(
    spec,
    child,
    HEALTH_REQUEST_TIMEOUT_MS,
  );
  if (!result.ok) {
    throw new Error(
      `${spec.target} ${context} health check failed: ${result.failure}`,
    );
  }
  const validationFailure = healthValidationFailure(spec, result.capture);
  if (validationFailure != null) {
    throw new Error(
      `${spec.target} ${context} health check failed: ${validationFailure}`,
    );
  }
  return result.capture;
}

function healthValidationFailure(
  spec: LaunchSpec,
  capture: HealthCapture,
): string | null {
  if (spec.target !== "pocket") return null;
  try {
    validatePocketHealthBody(capture.body);
    return null;
  } catch (error) {
    return errorMessage(error);
  }
}

export function validatePocketHealthBody(body: JsonObject): number {
  if (!isJsonObject(body.status)) {
    throw new Error("Pocket health status must be an object");
  }
  const status = body.status;
  if (status.modelConfigured !== true) {
    throw new Error("Pocket health modelConfigured must be true");
  }
  if (status.windowVisible !== true) {
    throw new Error("Pocket health windowVisible must be true");
  }
  if (
    typeof status.renderFps !== "number" ||
    !Number.isFinite(status.renderFps) ||
    status.renderFps <= 1
  ) {
    throw new Error("Pocket health renderFps must be a finite number > 1");
  }
  return status.renderFps;
}

async function probeHealth(
  spec: LaunchSpec,
  child: SpawnedProcess,
  timeoutMs: number,
): Promise<
  | { ok: true; capture: HealthCapture }
  | { ok: false; failure: string }
> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  const started = performance.now();
  const request = (async () => {
    try {
      const response = await fetch(spec.healthUrl, {
        headers: { accept: "application/json" },
        signal: controller.signal,
      });
      const text = await response.text();
      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch {
        return {
          ok: false as const,
          failure: `HTTP ${response.status} returned invalid JSON: ${text.slice(0, 200)}`,
        };
      }
      if (!isJsonObject(parsed)) {
        return {
          ok: false as const,
          failure: `HTTP ${response.status} returned a non-object JSON body`,
        };
      }
      if (response.status !== 200 || parsed.ok !== true) {
        return {
          ok: false as const,
          failure:
            `HTTP ${response.status} health ok=${String(parsed.ok)}: ` +
            text.slice(0, 200),
        };
      }
      return {
        ok: true as const,
        capture: {
          captured_at: new Date().toISOString(),
          response_time_ms: round(performance.now() - started),
          http_status: response.status,
          body: parsed,
        },
      };
    } catch (error) {
      return {
        ok: false as const,
        failure:
          controller.signal.aborted
            ? `request exceeded ${round(timeoutMs)}ms`
            : errorMessage(error),
      };
    }
  })();

  const outcome = await Promise.race([
    request.then((result) => ({ kind: "health" as const, result })),
    child.exited.then((exitCode) => ({
      kind: "exit" as const,
      exitCode,
    })),
  ]);
  clearTimeout(timeout);
  if (outcome.kind === "exit") {
    controller.abort();
    throw new Error(
      `${spec.target} exited before health readiness (exit ${outcome.exitCode})`,
    );
  }
  return outcome.result;
}

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

interface CdpTarget {
  id: string;
  title: string;
  url: string;
  webSocketDebuggerUrl: string;
}

interface PreparedReferenceCdp {
  client: CdpClient;
  receipt: ReferenceCdpReceipt;
}

interface ActiveReferenceFrameMeasurement {
  requestedDurationMs: number;
  beforeMetrics: Record<string, number>;
  framePromise: Promise<ReferenceFrameSample>;
}

interface CdpPending {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timeout: ReturnType<typeof setTimeout>;
}

class CdpClient {
  private nextId = 1;
  private readonly pending = new Map<number, CdpPending>();
  private closed = false;

  constructor(private readonly socket: WebSocket) {
    socket.addEventListener("message", (event) => {
      void this.handleMessage(event.data);
    });
    socket.addEventListener("close", () => {
      this.failPending(new Error("CDP socket closed"));
    });
    socket.addEventListener("error", () => {
      this.failPending(new Error("CDP socket error"));
    });
  }

  send<T>(
    method: string,
    params: JsonObject = {},
    timeoutMs = CDP_COMMAND_TIMEOUT_MS,
  ): Promise<T> {
    if (this.closed || this.socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error(`CDP socket is not open for ${method}`));
    }
    const id = this.nextId++;
    return new Promise<T>((resolveSend, rejectSend) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        rejectSend(new Error(`CDP ${method} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (value) => resolveSend(value as T),
        reject: rejectSend,
        timeout,
      });
      try {
        this.socket.send(JSON.stringify({ id, method, params }));
      } catch (error) {
        clearTimeout(timeout);
        this.pending.delete(id);
        rejectSend(new Error(`CDP ${method} send failed: ${errorMessage(error)}`));
      }
    });
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.failPending(new Error("CDP client closed"));
    this.socket.close();
  }

  private async handleMessage(data: unknown): Promise<void> {
    let text: string;
    if (typeof data === "string") text = data;
    else if (data instanceof ArrayBuffer) text = new TextDecoder().decode(data);
    else if (data instanceof Blob) text = await data.text();
    else return;

    let message: unknown;
    try {
      message = JSON.parse(text);
    } catch {
      this.failPending(new Error("CDP returned invalid JSON"));
      return;
    }
    if (!isJsonObject(message) || typeof message.id !== "number") return;
    const pending = this.pending.get(message.id);
    if (!pending) return;
    this.pending.delete(message.id);
    clearTimeout(pending.timeout);
    if (message.error != null) {
      pending.reject(
        new Error(`CDP command failed: ${JSON.stringify(message.error)}`),
      );
    } else {
      pending.resolve(message.result ?? {});
    }
  }

  private failPending(error: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timeout);
      pending.reject(error);
    }
    this.pending.clear();
  }
}

async function prepareReferenceCdp(
  spec: LaunchSpec,
  child: SpawnedProcess,
): Promise<PreparedReferenceCdp> {
  if (spec.cdpListUrl == null || spec.cdpPort == null) {
    throw new Error("reference launch is missing its CDP endpoint");
  }
  const deadline = performance.now() + CDP_TIMEOUT_SECONDS * 1_000;
  let lastFailure = "CDP target list has not responded";
  let target: CdpTarget | null = null;

  while (performance.now() < deadline && target == null) {
    try {
      const raw = await fetchJsonWhileRunning(
        spec.cdpListUrl,
        spec,
        child,
        Math.min(
          HEALTH_REQUEST_TIMEOUT_MS,
          Math.max(1, deadline - performance.now()),
        ),
      );
      target = selectReferenceCdpTarget(raw);
      if (target == null) lastFailure = "no non-settings page target";
    } catch (error) {
      lastFailure = errorMessage(error);
    }
    if (target == null) {
      await delayWhileRunning(
        child,
        Math.min(
          CDP_POLL_INTERVAL_MS,
          Math.max(0, deadline - performance.now()),
        ),
        "reference exited before CDP target readiness",
      );
    }
  }
  if (target == null) {
    throw new Error(
      `reference CDP target timed out after ${CDP_TIMEOUT_SECONDS}s: ${lastFailure}`,
    );
  }

  const client = await connectCdp(target.webSocketDebuggerUrl, spec, child);
  try {
    await client.send("Runtime.enable");
    await client.send("Performance.enable");
    await client.send("Emulation.setDeviceMetricsOverride", {
      width: REFERENCE_VIEWPORT.width,
      height: REFERENCE_VIEWPORT.height,
      deviceScaleFactor: REFERENCE_VIEWPORT.deviceScaleFactor,
      mobile: false,
    });
    const canvas = await waitForReferenceCanvas(client, child);
    return {
      client,
      receipt: {
        ready_at: new Date().toISOString(),
        target: {
          id: target.id,
          title: target.title,
          url: target.url,
        },
        canvas,
      },
    };
  } catch (error) {
    client.close();
    throw error;
  }
}

async function fetchJsonWhileRunning(
  url: string,
  spec: LaunchSpec,
  child: SpawnedProcess,
  timeoutMs: number,
): Promise<unknown> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  const request = (async () => {
    const response = await fetch(url, {
      headers: { accept: "application/json" },
      signal: controller.signal,
    });
    if (!response.ok) throw new Error(`HTTP ${response.status} from ${url}`);
    return response.json() as Promise<unknown>;
  })();
  const outcome = await Promise.race([
    request.then(
      (value) => ({ kind: "response" as const, value }),
      (error) => ({ kind: "error" as const, error }),
    ),
    child.exited.then((exitCode) => ({ kind: "exit" as const, exitCode })),
  ]);
  clearTimeout(timeout);
  if (outcome.kind === "exit") {
    controller.abort();
    throw new Error(
      `${spec.target} exited before CDP readiness (exit ${outcome.exitCode})`,
    );
  }
  if (outcome.kind === "error") throw outcome.error;
  return outcome.value;
}

function selectReferenceCdpTarget(value: unknown): CdpTarget | null {
  if (!Array.isArray(value)) return null;
  for (const candidate of value) {
    if (
      !isJsonObject(candidate) ||
      candidate.type !== "page" ||
      typeof candidate.id !== "string" ||
      typeof candidate.title !== "string" ||
      typeof candidate.url !== "string" ||
      typeof candidate.webSocketDebuggerUrl !== "string"
    ) {
      continue;
    }
    const identity = `${candidate.title} ${candidate.url}`.toLowerCase();
    if (identity.includes("settings") || candidate.url.startsWith("devtools://")) {
      continue;
    }
    return {
      id: candidate.id,
      title: candidate.title,
      url: candidate.url,
      webSocketDebuggerUrl: candidate.webSocketDebuggerUrl,
    };
  }
  return null;
}

async function connectCdp(
  url: string,
  spec: LaunchSpec,
  child: SpawnedProcess,
): Promise<CdpClient> {
  const socket = new WebSocket(url);
  const opened = new Promise<
    { kind: "open" } | { kind: "error"; error: Error }
  >((resolveOpen) => {
    socket.addEventListener(
      "open",
      () => resolveOpen({ kind: "open" }),
      { once: true },
    );
    socket.addEventListener(
      "error",
      () =>
        resolveOpen({
          kind: "error",
          error: new Error(`unable to connect CDP socket ${url}`),
        }),
      { once: true },
    );
  });
  let timeoutId: ReturnType<typeof setTimeout>;
  const timeout = new Promise<{ kind: "timeout" }>((resolveTimeout) => {
    timeoutId = setTimeout(
      () => resolveTimeout({ kind: "timeout" }),
      CDP_COMMAND_TIMEOUT_MS,
    );
  });
  const outcome = await Promise.race([
    opened,
    timeout,
    child.exited.then((exitCode) => ({ kind: "exit" as const, exitCode })),
  ]);
  clearTimeout(timeoutId!);
  if (outcome.kind !== "open") {
    socket.close();
    if (outcome.kind === "exit") {
      throw new Error(
        `${spec.target} exited before CDP socket readiness (exit ${outcome.exitCode})`,
      );
    }
    if (outcome.kind === "error") throw outcome.error;
    throw new Error(`CDP socket connection timed out after ${CDP_COMMAND_TIMEOUT_MS}ms`);
  }
  return new CdpClient(socket);
}

function referenceCanvasExpression(): string {
  return `(() => {
  const canvas = document.querySelector("canvas");
  const gl = canvas?.getContext("webgl2") ?? canvas?.getContext("webgl");
  const debug = gl?.getExtension("WEBGL_debug_renderer_info");
  return {
    ready_state: document.readyState,
    viewport: [innerWidth, innerHeight, devicePixelRatio],
    canvas: canvas ? {
      client: [canvas.clientWidth, canvas.clientHeight],
      backing: [canvas.width, canvas.height],
    } : null,
    webgl: gl ? {
      version: String(gl.getParameter(gl.VERSION)),
      renderer: String(debug ? gl.getParameter(debug.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER)),
      vendor: String(debug ? gl.getParameter(debug.UNMASKED_VENDOR_WEBGL) : gl.getParameter(gl.VENDOR)),
    } : null,
  };
})()`;
}

async function waitForReferenceCanvas(
  client: CdpClient,
  child: SpawnedProcess,
): Promise<ReferenceCanvasReceipt> {
  const deadline = performance.now() + CDP_TIMEOUT_SECONDS * 1_000;
  let lastFailure = "document and canvas are not ready";
  while (performance.now() < deadline) {
    try {
      const evaluation = await client.send<unknown>("Runtime.evaluate", {
        expression: referenceCanvasExpression(),
        returnByValue: true,
      });
      const value = runtimeEvaluationValue(evaluation, "reference canvas");
      return parseReferenceCanvasReceipt(value);
    } catch (error) {
      lastFailure = errorMessage(error);
    }
    await delayWhileRunning(
      child,
      Math.min(
        CDP_POLL_INTERVAL_MS,
        Math.max(0, deadline - performance.now()),
      ),
      "reference exited before canvas readiness",
    );
  }
  throw new Error(
    `reference canvas validation timed out after ${CDP_TIMEOUT_SECONDS}s: ${lastFailure}`,
  );
}

export function parseReferenceCanvasReceipt(
  value: unknown,
): ReferenceCanvasReceipt {
  if (!isJsonObject(value)) throw new Error("canvas receipt is not an object");
  if (value.ready_state !== "complete") {
    throw new Error(`document readyState is ${String(value.ready_state)}`);
  }
  const viewport = numberTuple(value.viewport, 3, "viewport");
  if (
    viewport[0] !== REFERENCE_VIEWPORT.width ||
    viewport[1] !== REFERENCE_VIEWPORT.height ||
    viewport[2] !== REFERENCE_VIEWPORT.deviceScaleFactor
  ) {
    throw new Error(`unexpected viewport ${viewport.join("x")}`);
  }
  if (!isJsonObject(value.canvas)) throw new Error("canvas is missing");
  const client = numberTuple(value.canvas.client, 2, "canvas client");
  const backing = numberTuple(value.canvas.backing, 2, "canvas backing");
  if (
    backing[0] !== REFERENCE_CANVAS_BACKING[0] ||
    backing[1] !== REFERENCE_CANVAS_BACKING[1]
  ) {
    throw new Error(`unexpected canvas backing ${backing.join("x")}`);
  }
  if (!isJsonObject(value.webgl)) throw new Error("WebGL context is missing");
  const version = requiredString(value.webgl.version, "WebGL version");
  const renderer = requiredString(value.webgl.renderer, "WebGL renderer");
  const vendor = requiredString(value.webgl.vendor, "WebGL vendor");
  return {
    ready_state: "complete",
    viewport: [viewport[0], viewport[1], viewport[2]],
    canvas: {
      client: [client[0], client[1]],
      backing: [backing[0], backing[1]],
    },
    webgl: { version, renderer, vendor },
  };
}

function runtimeEvaluationValue(value: unknown, context: string): unknown {
  if (!isJsonObject(value)) throw new Error(`${context} CDP result is invalid`);
  if (value.exceptionDetails != null) {
    throw new Error(`${context} evaluation failed: ${JSON.stringify(value.exceptionDetails)}`);
  }
  if (!isJsonObject(value.result) || !("value" in value.result)) {
    throw new Error(`${context} evaluation returned no value`);
  }
  return value.result.value;
}

function numberTuple(
  value: unknown,
  length: number,
  name: string,
): number[] {
  if (
    !Array.isArray(value) ||
    value.length !== length ||
    value.some((item) => typeof item !== "number" || !Number.isFinite(item))
  ) {
    throw new Error(`${name} must contain ${length} finite numbers`);
  }
  return value;
}

function requiredString(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} is missing`);
  }
  return value;
}

async function startReferenceFrameMeasurement(
  client: CdpClient,
  durationMs: number,
): Promise<ActiveReferenceFrameMeasurement> {
  if (!Number.isFinite(durationMs) || durationMs <= 0) {
    throw new Error(`invalid reference frame duration ${durationMs}`);
  }
  const beforeMetrics = performanceMetricMap(
    await client.send("Performance.getMetrics"),
  );
  const expression = `new Promise((resolve) => {
    const intervals = [];
    const start = performance.now();
    let last = start;
    function tick(now) {
      intervals.push(now - last);
      last = now;
      if (now - start < ${JSON.stringify(durationMs)}) {
        requestAnimationFrame(tick);
        return;
      }
      const sorted = intervals.slice().sort((a, b) => a - b);
      const percentile = (p) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))] ?? null;
      let total = 0;
      let over20 = 0;
      for (const interval of intervals) {
        total += interval;
        if (interval > 20) over20++;
      }
      const elapsed = now - start;
      resolve({
        requested_duration_ms: ${JSON.stringify(durationMs)},
        elapsed_ms: elapsed,
        frames: intervals.length,
        fps: intervals.length / (elapsed / 1000),
        mean_interval_ms: total / intervals.length,
        p50_interval_ms: percentile(0.50),
        p95_interval_ms: percentile(0.95),
        p99_interval_ms: percentile(0.99),
        max_interval_ms: sorted.at(-1) ?? null,
        over_20ms: over20,
      });
    }
    requestAnimationFrame(tick);
  })`;
  const framePromise = client
    .send<unknown>(
      "Runtime.evaluate",
      {
        expression,
        awaitPromise: true,
        returnByValue: true,
      },
      durationMs + CDP_COMMAND_TIMEOUT_MS,
    )
    .then((result) =>
      parseReferenceFrameSample(
        runtimeEvaluationValue(result, "reference frame sample"),
        durationMs,
      ),
    );
  void framePromise.catch(() => undefined);
  return { requestedDurationMs: durationMs, beforeMetrics, framePromise };
}

async function finishReferenceFrameMeasurement(
  client: CdpClient,
  active: ActiveReferenceFrameMeasurement,
): Promise<ReferenceFrameReceipt> {
  const frameSample = await active.framePromise;
  const afterMetrics = performanceMetricMap(
    await client.send("Performance.getMetrics"),
  );
  const names = [
    "TaskDuration",
    "ScriptDuration",
    "LayoutDuration",
    "RecalcStyleDuration",
    "V8CompileDuration",
  ] as const;
  const deltas = Object.fromEntries(
    names.map((name) => [
      name,
      round(
        name === "TaskDuration"
          ? requiredMetric(afterMetrics, name) -
              requiredMetric(active.beforeMetrics, name)
          : (afterMetrics[name] ?? 0) -
              (active.beforeMetrics[name] ?? 0),
      ),
    ]),
  ) as ReferenceFrameReceipt["performance_deltas_seconds"];
  if (deltas.TaskDuration < 0) {
    throw new Error(`negative CDP TaskDuration delta ${deltas.TaskDuration}`);
  }
  const mainThreadCpu = round(
    (100 * deltas.TaskDuration) / (frameSample.elapsed_ms / 1_000),
  );
  if (!Number.isFinite(mainThreadCpu)) {
    throw new Error("invalid CDP main-thread CPU result");
  }
  return {
    captured_at: new Date().toISOString(),
    frame_sample: frameSample,
    task_duration_delta_seconds: deltas.TaskDuration,
    main_thread_cpu_percent: mainThreadCpu,
    performance_deltas_seconds: deltas,
  };
}

function performanceMetricMap(value: unknown): Record<string, number> {
  if (!isJsonObject(value) || !Array.isArray(value.metrics)) {
    throw new Error("CDP Performance.getMetrics returned no metrics");
  }
  const result: Record<string, number> = {};
  for (const metric of value.metrics) {
    if (
      isJsonObject(metric) &&
      typeof metric.name === "string" &&
      typeof metric.value === "number" &&
      Number.isFinite(metric.value)
    ) {
      result[metric.name] = metric.value;
    }
  }
  return result;
}

function requiredMetric(metrics: Record<string, number>, name: string): number {
  const value = metrics[name];
  if (value == null || !Number.isFinite(value)) {
    throw new Error(`CDP metric ${name} is missing`);
  }
  return value;
}

export function parseReferenceFrameSample(
  value: unknown,
  requestedDurationMs: number,
): ReferenceFrameSample {
  if (!isJsonObject(value)) throw new Error("frame sample is not an object");
  const finite = (name: string): number => {
    const item = value[name];
    if (typeof item !== "number" || !Number.isFinite(item)) {
      throw new Error(`frame sample ${name} is invalid`);
    }
    return item;
  };
  const sample: ReferenceFrameSample = {
    requested_duration_ms: finite("requested_duration_ms"),
    elapsed_ms: finite("elapsed_ms"),
    frames: finite("frames"),
    fps: finite("fps"),
    mean_interval_ms: finite("mean_interval_ms"),
    p50_interval_ms: finite("p50_interval_ms"),
    p95_interval_ms: finite("p95_interval_ms"),
    p99_interval_ms: finite("p99_interval_ms"),
    max_interval_ms: finite("max_interval_ms"),
    over_20ms: finite("over_20ms"),
  };
  if (
    sample.requested_duration_ms !== requestedDurationMs ||
    sample.elapsed_ms < requestedDurationMs * 0.9 ||
    !Number.isInteger(sample.frames) ||
    sample.frames < 2 ||
    sample.fps <= 1 ||
    sample.mean_interval_ms <= 0 ||
    sample.p50_interval_ms <= 0 ||
    sample.p95_interval_ms <= 0 ||
    sample.p99_interval_ms <= 0 ||
    sample.max_interval_ms <= 0 ||
    !Number.isInteger(sample.over_20ms) ||
    sample.over_20ms < 0
  ) {
    throw new Error(`reference FPS receipt failed validation: ${JSON.stringify(sample)}`);
  }
  return sample;
}

async function benchmarkTarget(
  spec: LaunchSpec,
  options: Options,
  onStart: (run: RunResult) => void,
): Promise<void> {
  console.error(`bench-persona: launching ${spec.target}: ${JSON.stringify(spec.argv)}`);
  const child = Bun.spawn(spec.argv, {
    cwd: spec.cwd,
    env: {
      ...process.env,
      ...spec.envOverrides,
    },
    stdin: "ignore",
    // Keep stdout available for a JSON report while still exposing app logs.
    stdout: 2,
    stderr: 2,
    detached: true,
  });
  const run: RunResult = {
    status: "running",
    root_pid: child.pid,
    launched_at: new Date().toISOString(),
    ready_at: null,
    readiness_health: null,
    reference_cdp: null,
    frame_receipt: null,
    settled_at: null,
    sampling_completed_at: null,
    terminated_at: null,
    baseline: null,
    samples: [],
    summary: null,
  };
  onStart(run);
  activeRun = { target: spec.target, child };
  let referenceCdp: CdpClient | null = null;
  let frameMeasurement: ActiveReferenceFrameMeasurement | null = null;

  try {
    console.error(
      `bench-persona: ${spec.target} pid=${child.pid}; waiting for ${spec.healthUrl}`,
    );
    const readiness = await waitForReadyHealth(spec, child);
    run.ready_at = readiness.captured_at;
    run.readiness_health = readiness;
    if (spec.target === "reference") {
      const prepared = await prepareReferenceCdp(spec, child);
      referenceCdp = prepared.client;
      run.reference_cdp = prepared.receipt;
      console.error(
        `bench-persona: reference CDP canvas ready ` +
          `${prepared.receipt.canvas.canvas.backing.join("x")}`,
      );
    }
    console.error(
      `bench-persona: ${spec.target} ready; settling ${options.settleSeconds}s`,
    );
    await delayWhileRunning(
      child,
      options.settleSeconds * 1_000,
      `${spec.target} exited during settle`,
    );
    run.settled_at = new Date().toISOString();

    if (referenceCdp != null) {
      frameMeasurement = await startReferenceFrameMeasurement(
        referenceCdp,
        options.sampleCount * options.intervalSeconds * 1_000,
      );
    }
    let previous = await captureProcessTree(child.pid);
    run.baseline = previous.snapshot;
    const samplingStart = previous.clock_ms;
    console.error(
      `bench-persona: ${spec.target} baseline ` +
        `cpu-time=${previous.snapshot.cumulative_cpu_time_seconds.toFixed(2)}s ` +
        `rss=${formatMiB(previous.snapshot.rss_kib)} ` +
        `processes=${previous.snapshot.process_count}`,
    );

    for (let index = 0; index < options.sampleCount; index++) {
      const scheduledAt =
        samplingStart + (index + 1) * options.intervalSeconds * 1_000;
      await delayWhileRunning(
        child,
        Math.max(0, scheduledAt - performance.now()),
        `${spec.target} exited between samples`,
      );
      const current = await captureProcessTree(child.pid);
      const health = await captureRequiredHealth(
        spec,
        child,
        `sample ${index + 1}`,
      );
      const sample = buildIntervalSample(
        index + 1,
        samplingStart,
        previous,
        current,
        health,
      );
      run.samples.push(sample);
      console.error(
        `bench-persona: ${spec.target} ${index + 1}/${options.sampleCount} ` +
          `interval-cpu=${sample.interval_cpu_percent.toFixed(1)}% ` +
          `ps-cpu=${sample.ps_cpu_percent_sum_diagnostic.toFixed(1)}% ` +
          `rss=${formatMiB(sample.rss_kib)} ` +
          `processes=${sample.process_count}`,
      );
      previous = current;
    }

    run.sampling_completed_at = new Date().toISOString();
    if (referenceCdp != null && frameMeasurement != null) {
      run.frame_receipt = await finishReferenceFrameMeasurement(
        referenceCdp,
        frameMeasurement,
      );
      console.error(
        `bench-persona: reference delivered ` +
          `${run.frame_receipt.frame_sample.fps.toFixed(1)} fps`,
      );
    }
    if (spec.target === "reference" && run.frame_receipt == null) {
      throw new Error("reference frame receipt is missing");
    }
    run.summary = summarizeSamples(run.samples, spec.target);
    run.status = "ok";
  } catch (error) {
    run.status = "failed";
    run.error = errorMessage(error);
    throw error;
  } finally {
    referenceCdp?.close();
    await terminateProcessTree(child, spec.target);
    run.terminated_at = new Date().toISOString();
    if (activeRun?.child.pid === child.pid) activeRun = null;
  }
}

async function captureProcessTree(rootPid: number): Promise<TimedTreeSnapshot> {
  const clockMs = performance.now();
  const capturedAt = new Date().toISOString();
  const table = await readProcessTable();
  const processes = selectProcessTree(table, rootPid);
  if (processes.length === 0) {
    throw new Error(`benchmark process ${rootPid} is no longer present`);
  }
  return {
    clock_ms: clockMs,
    snapshot: {
      captured_at: capturedAt,
      root_pid: rootPid,
      cumulative_cpu_time_seconds: round(
        processes.reduce(
          (total, process) => total + process.cpu_time_seconds,
          0,
        ),
      ),
      ps_cpu_percent_sum_diagnostic: round(
        processes.reduce(
          (total, process) => total + process.ps_cpu_percent,
          0,
        ),
      ),
      rss_kib: processes.reduce(
        (total, process) => total + process.rss_kib,
        0,
      ),
      process_count: processes.length,
      processes,
    },
  };
}

function buildIntervalSample(
  index: number,
  samplingStart: number,
  previous: TimedTreeSnapshot,
  current: TimedTreeSnapshot,
  health: HealthCapture,
): ResourceSample {
  const intervalSeconds = (current.clock_ms - previous.clock_ms) / 1_000;
  if (!(intervalSeconds > 0)) {
    throw new Error(`sample ${index} has a non-positive interval`);
  }

  const previousByPid = new Map(
    previous.snapshot.processes.map((process) => [process.pid, process]),
  );
  const currentPids = new Set(
    current.snapshot.processes.map((process) => process.pid),
  );
  const processes: IntervalProcessRow[] = current.snapshot.processes.map(
    (process) => {
      const before = previousByPid.get(process.pid);
      const continued =
        before != null &&
        before.ppid === process.ppid &&
        before.command === process.command &&
        process.cpu_time_seconds >= before.cpu_time_seconds;
      const cpuTime = continued
        ? process.cpu_time_seconds - before.cpu_time_seconds
        : process.cpu_time_seconds;
      return {
        ...process,
        interval_cpu_time_seconds: round(cpuTime),
        interval_cpu_percent: round((cpuTime / intervalSeconds) * 100),
        interval_identity: continued ? "continued" : "new-or-restarted",
      };
    },
  );
  const intervalCpuTime = processes.reduce(
    (total, process) => total + process.interval_cpu_time_seconds,
    0,
  );
  const disappearedProcesses = previous.snapshot.processes.filter(
    (process) => !currentPids.has(process.pid),
  );
  if (disappearedProcesses.length > 0) {
    console.error(
      `bench-persona: sample ${index} cannot observe post-snapshot CPU for ` +
        `${disappearedProcesses.length} exited process(es)`,
    );
  }

  return {
    index,
    interval_started_at: previous.snapshot.captured_at,
    captured_at: current.snapshot.captured_at,
    elapsed_since_baseline_seconds: round(
      (current.clock_ms - samplingStart) / 1_000,
    ),
    interval_seconds: round(intervalSeconds),
    root_pid: current.snapshot.root_pid,
    interval_cpu_time_seconds: round(intervalCpuTime),
    interval_cpu_percent: round((intervalCpuTime / intervalSeconds) * 100),
    cumulative_cpu_time_seconds:
      current.snapshot.cumulative_cpu_time_seconds,
    ps_cpu_percent_sum_diagnostic:
      current.snapshot.ps_cpu_percent_sum_diagnostic,
    rss_kib: current.snapshot.rss_kib,
    process_count: current.snapshot.process_count,
    processes,
    disappeared_processes: disappearedProcesses,
    health,
  };
}

async function readProcessTable(): Promise<ProcessRow[]> {
  const ps = Bun.spawn(
    [
      "ps",
      "-axo",
      "pid=,ppid=,state=,time=,%cpu=,rss=,command=",
      "-ww",
    ],
    {
      env: {
        ...process.env,
        LC_ALL: "C",
        LANG: "C",
      },
      stdin: "ignore",
      stdout: "pipe",
      stderr: "pipe",
    },
  );
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(ps.stdout).text(),
    new Response(ps.stderr).text(),
    ps.exited,
  ]);
  if (exitCode !== 0) {
    throw new Error(`ps failed (${exitCode}): ${stderr.trim() || "no stderr"}`);
  }

  const rows: ProcessRow[] = [];
  for (const line of stdout.split("\n")) {
    const match =
      /^\s*(\d+)\s+(\d+)\s+(\S+)\s+(\S+)\s+([0-9]+(?:\.[0-9]+)?)\s+(\d+)\s*(.*)$/.exec(
        line,
      );
    if (!match) continue;
    const cpuTimeSeconds = parseCpuTimeSeconds(match[4]);
    rows.push({
      pid: Number(match[1]),
      ppid: Number(match[2]),
      state: match[3],
      cpu_time: match[4],
      cpu_time_seconds: cpuTimeSeconds,
      ps_cpu_percent: Number(match[5]),
      rss_kib: Number(match[6]),
      command: match[7],
    });
  }
  if (rows.length === 0) throw new Error("ps returned no parseable processes");
  return rows;
}

export function parseCpuTimeSeconds(value: string): number {
  const dayParts = value.split("-");
  if (dayParts.length > 2) throw new Error(`invalid ps CPU time ${value}`);
  const days = dayParts.length === 2 ? Number(dayParts[0]) : 0;
  const clock = dayParts.at(-1) ?? "";
  const parts = clock.split(":").map(Number);
  if (
    !Number.isInteger(days) ||
    days < 0 ||
    (parts.length !== 2 && parts.length !== 3) ||
    parts.some((part) => !Number.isFinite(part) || part < 0)
  ) {
    throw new Error(`invalid ps CPU time ${value}`);
  }

  let hours = 0;
  let minutes: number;
  let seconds: number;
  if (parts.length === 3) {
    [hours, minutes, seconds] = parts;
    if (minutes >= 60) throw new Error(`invalid ps CPU time ${value}`);
  } else {
    [minutes, seconds] = parts;
  }
  if (seconds >= 60) throw new Error(`invalid ps CPU time ${value}`);
  return days * 86_400 + hours * 3_600 + minutes * 60 + seconds;
}

function selectProcessTree(table: ProcessRow[], rootPid: number): ProcessRow[] {
  const byPid = new Map(table.map((row) => [row.pid, row]));
  const root = byPid.get(rootPid);
  if (!root || root.state.startsWith("Z")) return [];

  const children = new Map<number, ProcessRow[]>();
  for (const row of table) {
    const siblings = children.get(row.ppid);
    if (siblings) siblings.push(row);
    else children.set(row.ppid, [row]);
  }
  for (const siblings of children.values()) {
    siblings.sort((left, right) => left.pid - right.pid);
  }

  const result: ProcessRow[] = [];
  const seen = new Set<number>();
  const visit = (pid: number): void => {
    if (seen.has(pid)) return;
    seen.add(pid);
    const row = byPid.get(pid);
    if (!row) return;
    if (!row.state.startsWith("Z")) result.push(row);
    for (const child of children.get(pid) ?? []) visit(child.pid);
  };
  visit(rootPid);
  return result;
}

async function delayWhileRunning(
  child: SpawnedProcess,
  milliseconds: number,
  context: string,
): Promise<void> {
  if (milliseconds <= 0) {
    assertProcessAlive(child.pid, context);
    return;
  }
  const outcome = await Promise.race([
    Bun.sleep(milliseconds).then(() => ({ kind: "elapsed" as const })),
    child.exited.then((exitCode) => ({
      kind: "exit" as const,
      exitCode,
    })),
  ]);
  if (outcome.kind === "exit") {
    throw new Error(`${context} (exit ${outcome.exitCode})`);
  }
  assertProcessAlive(child.pid, context);
}

function assertProcessAlive(pid: number, context: string): void {
  if (!isProcessAlive(pid)) throw new Error(context);
}

export async function terminateProcessTree(
  child: SpawnedProcess,
  target: TargetName,
): Promise<void> {
  const existing = terminationPromises.get(child.pid);
  if (existing) return existing;

  const termination = terminateProcessTreeInner(child, target);
  terminationPromises.set(child.pid, termination);
  try {
    await termination;
  } finally {
    terminationPromises.delete(child.pid);
  }
}

async function terminateProcessTreeInner(
  child: SpawnedProcess,
  target: TargetName,
): Promise<void> {
  const rootPid = child.pid;
  if (rootPid <= 1 || rootPid === process.pid) {
    throw new Error(`refusing to terminate unsafe pid ${rootPid}`);
  }

  let knownTree: ProcessRow[] = [];
  try {
    knownTree = selectProcessTree(await readProcessTable(), rootPid);
  } catch (error) {
    console.error(
      `bench-persona: unable to snapshot ${target} tree: ${errorMessage(error)}`,
    );
  }
  const knownPids = new Set([rootPid, ...knownTree.map((row) => row.pid)]);

  signalProcessGroup(rootPid, "SIGTERM");
  signalPids([...knownPids].reverse(), "SIGTERM");

  if (await waitUntilStopped(rootPid, knownPids, TERM_GRACE_MS)) {
    await reapChild(child);
    return;
  }

  console.error(`bench-persona: ${target} did not exit after SIGTERM; sending SIGKILL`);
  try {
    for (const row of selectProcessTree(await readProcessTable(), rootPid)) {
      knownPids.add(row.pid);
    }
  } catch {
    // The root may already be gone while a previously observed helper remains.
  }
  signalProcessGroup(rootPid, "SIGKILL");
  signalPids([...knownPids].reverse(), "SIGKILL");

  const stopped = await waitUntilStopped(rootPid, knownPids, KILL_GRACE_MS);
  await reapChild(child);
  if (!stopped) {
    const survivors = [...knownPids].filter(isProcessAlive);
    console.error(
      `bench-persona: warning: ${target} survivors after SIGKILL: ` +
        (survivors.length ? survivors.join(", ") : `process group ${rootPid}`),
    );
  }
}

function signalProcessGroup(
  rootPid: number,
  signal: NodeJS.Signals,
): void {
  try {
    process.kill(-rootPid, signal);
  } catch (error) {
    if (!isMissingProcess(error)) {
      console.error(
        `bench-persona: unable to signal process group ${rootPid}: ${errorMessage(error)}`,
      );
    }
  }
}

function signalPids(pids: number[], signal: NodeJS.Signals): void {
  for (const pid of pids) {
    if (pid <= 1 || pid === process.pid) continue;
    try {
      process.kill(pid, signal);
    } catch (error) {
      if (!isMissingProcess(error)) {
        console.error(
          `bench-persona: unable to signal pid ${pid}: ${errorMessage(error)}`,
        );
      }
    }
  }
}

async function waitUntilStopped(
  processGroup: number,
  pids: Set<number>,
  timeoutMs: number,
): Promise<boolean> {
  const deadline = performance.now() + timeoutMs;
  while (performance.now() < deadline) {
    if (
      !isProcessGroupAlive(processGroup) &&
      ![...pids].some(isProcessAlive)
    ) {
      return true;
    }
    await Bun.sleep(POLL_MS);
  }
  return (
    !isProcessGroupAlive(processGroup) && ![...pids].some(isProcessAlive)
  );
}

async function reapChild(child: SpawnedProcess): Promise<void> {
  await Promise.race([
    child.exited.then(() => undefined),
    Bun.sleep(POLL_MS * 5),
  ]);
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return !isMissingProcess(error);
  }
}

function isProcessGroupAlive(processGroup: number): boolean {
  try {
    process.kill(-processGroup, 0);
    return true;
  } catch (error) {
    return !isMissingProcess(error);
  }
}

function isMissingProcess(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    error.code === "ESRCH"
  );
}

function summarizeSamples(
  samples: ResourceSample[],
  target: TargetName,
): RunSummary {
  if (samples.length === 0) throw new Error("cannot summarize zero samples");
  return {
    interval_cpu_percent: summarize(
      samples.map((sample) => sample.interval_cpu_percent),
    ),
    rss_kib: summarize(samples.map((sample) => sample.rss_kib)),
    process_count: summarize(samples.map((sample) => sample.process_count)),
    render_fps:
      target === "pocket"
        ? summarize(
            samples.map((sample) =>
              validatePocketHealthBody(sample.health.body),
            ),
          )
        : null,
  };
}

function summarize(values: number[]): SummaryStats {
  const sorted = [...values].sort((left, right) => left - right);
  return {
    n: sorted.length,
    mean: round(values.reduce((sum, value) => sum + value, 0) / values.length),
    min: sorted[0],
    median: round(quantile(sorted, 0.5)),
    p95: round(quantile(sorted, 0.95)),
    max: sorted[sorted.length - 1],
  };
}

function quantile(sorted: number[], percentile: number): number {
  if (sorted.length === 1) return sorted[0];
  const position = (sorted.length - 1) * percentile;
  const lower = Math.floor(position);
  const upper = Math.ceil(position);
  const fraction = position - lower;
  return sorted[lower] * (1 - fraction) + sorted[upper] * fraction;
}

function compareRuns(referenceRun: RunResult, pocketRun: RunResult) {
  const reference = referenceRun.summary;
  const pocket = pocketRun.summary;
  const frameReceipt = referenceRun.frame_receipt;
  if (
    reference == null ||
    pocket == null ||
    frameReceipt == null ||
    pocket.render_fps == null
  ) {
    throw new Error("cannot compare incomplete frame-aware benchmark runs");
  }
  const referenceFps = frameReceipt.frame_sample.fps;
  const pocketFps = pocket.render_fps.median;
  const referenceCpuPerFrame =
    reference.interval_cpu_percent.median / referenceFps;
  const pocketCpuPerFrame =
    pocket.interval_cpu_percent.median / pocketFps;
  return {
    lower_is_better: true,
    reduction_percent_formula:
      "(reference - pocket) / reference * 100",
    ratio_formula: "reference / pocket",
    interval_cpu_percent: compareStats(
      reference.interval_cpu_percent,
      pocket.interval_cpu_percent,
    ),
    rss_kib: compareStats(reference.rss_kib, pocket.rss_kib),
    process_count: compareStats(reference.process_count, pocket.process_count),
    observed_fps: {
      reference: round(referenceFps),
      pocket: round(pocketFps),
      reference_source: "CDP requestAnimationFrame",
      pocket_source: "median health renderFps",
    },
    cpu_per_delivered_frame: {
      formula: "median interval CPU percent / observed fps",
      ...compareValue(referenceCpuPerFrame, pocketCpuPerFrame),
    },
  };
}

function compareStats(reference: SummaryStats, pocket: SummaryStats) {
  return {
    median: compareValue(reference.median, pocket.median),
    p95: compareValue(reference.p95, pocket.p95),
  };
}

function compareValue(reference: number, pocket: number) {
  return {
    reference,
    pocket,
    reduction_percent:
      reference === 0 ? null : round(((reference - pocket) / reference) * 100),
    ratio_reference_over_pocket:
      pocket === 0 ? null : round(reference / pocket),
  };
}

function parseArgs(argv: string[]): Options {
  if (argv.includes("--help") || argv.includes("-h")) {
    console.log(usage());
    process.exit(0);
  }

  const names = new Set([
    "--reference-bin",
    "--reference-root",
    "--pocket-bin",
    "--library",
    "--bundle",
    "--max-fps",
    "--max-texture-dim",
    "--settle",
    "--samples",
    "--interval",
    "--out",
  ]);
  const values = new Map<string, string>();

  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index];
    const equals = argument.indexOf("=");
    const name = equals >= 0 ? argument.slice(0, equals) : argument;
    if (!names.has(name)) throw new Error(`unknown argument ${argument}`);
    if (values.has(name)) throw new Error(`${name} may only be specified once`);

    let value: string;
    if (equals >= 0) {
      value = argument.slice(equals + 1);
    } else {
      const next = argv[index + 1];
      if (next == null) throw new Error(`${name} requires a value`);
      value = next;
      index++;
    }
    if (value.length === 0) throw new Error(`${name} requires a value`);
    values.set(name, value);
  }

  const required = (name: string): string => {
    const value = values.get(name);
    if (value == null) throw new Error(`missing required ${name}`);
    return value;
  };
  const numberValue = (
    name: string,
    fallback: number,
    minimum: number,
  ): number => {
    const raw = values.get(name);
    const value = raw == null ? fallback : Number(raw);
    if (!Number.isFinite(value) || value < minimum) {
      throw new Error(`${name} must be a number >= ${minimum}`);
    }
    return value;
  };

  const sampleCount = numberValue("--samples", DEFAULT_SAMPLE_COUNT, 1);
  if (!Number.isInteger(sampleCount)) {
    throw new Error("--samples must be an integer");
  }

  const maxFpsRaw = values.get("--max-fps");
  const maxFps = maxFpsRaw == null ? null : Number(maxFpsRaw);
  if (maxFps != null && (!Number.isFinite(maxFps) || maxFps <= 0)) {
    throw new Error("--max-fps must be a number > 0");
  }

  const maxTextureDimRaw = values.get("--max-texture-dim");
  const maxTextureDim =
    maxTextureDimRaw == null ? null : Number(maxTextureDimRaw);
  if (
    maxTextureDim != null &&
    (!Number.isInteger(maxTextureDim) || maxTextureDim <= 0)
  ) {
    throw new Error("--max-texture-dim must be an integer > 0");
  }

  const intervalSeconds = numberValue(
    "--interval",
    DEFAULT_INTERVAL_SECONDS,
    0,
  );
  if (intervalSeconds <= 0) {
    throw new Error("--interval must be a number > 0");
  }

  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const outRaw =
    values.get("--out") ??
    join(REPO_ROOT, "dist", "bench", `persona-ab-${stamp}.json`);
  return {
    referenceBin: executablePath(required("--reference-bin")),
    referenceRoot: absolutePath(required("--reference-root")),
    pocketBin: executablePath(required("--pocket-bin")),
    library: absolutePath(required("--library")),
    bundle: absolutePath(required("--bundle")),
    maxFps,
    maxTextureDim,
    settleSeconds: numberValue(
      "--settle",
      DEFAULT_SETTLE_SECONDS,
      0,
    ),
    sampleCount,
    intervalSeconds,
    outPath: outRaw === "-" ? "-" : absolutePath(outRaw),
  };
}

function validateInputs(options: Options): void {
  if (platform() === "win32") {
    throw new Error("bench-persona requires a POSIX ps implementation");
  }
  assertDirectory(options.referenceRoot, "--reference-root");
  assertExists(options.library, "--library");
  assertExists(options.bundle, "--bundle");
  assertExecutablePath(options.referenceBin, "--reference-bin");
  assertExecutablePath(options.pocketBin, "--pocket-bin");
  const referenceLibrary = join(
    options.referenceRoot,
    "public",
    "assets",
    "library.json",
  );
  assertExists(referenceLibrary, "Persona reference library");
  if (realpathSync(options.library) !== realpathSync(referenceLibrary)) {
    throw new Error(
      "--library must be the reference checkout's public/assets/library.json " +
        "so both targets consume the same catalog",
    );
  }
}

function assertDirectory(path: string, name: string): void {
  if (!existsSync(path) || !statSync(path).isDirectory()) {
    throw new Error(`${name} is not a directory: ${path}`);
  }
}

function assertExists(path: string, name: string): void {
  if (!existsSync(path)) throw new Error(`${name} does not exist: ${path}`);
}

function assertExecutablePath(value: string, name: string): void {
  if (!value.includes("/")) return;
  if (!existsSync(value) || !statSync(value).isFile()) {
    throw new Error(`${name} is not a file: ${value}`);
  }
}

function executablePath(value: string): string {
  const expanded = expandHome(value);
  return expanded.includes("/") ? resolve(expanded) : expanded;
}

function absolutePath(value: string): string {
  return resolve(expandHome(value));
}

function expandHome(value: string): string {
  if (value === "~") return homedir();
  if (value.startsWith("~/")) return join(homedir(), value.slice(2));
  return value;
}

function prepareOutput(outPath: string): void {
  if (outPath !== "-") mkdirSync(dirname(outPath), { recursive: true });
}

function writeReport(outPath: string, report: Report): void {
  const json = `${JSON.stringify(report, null, 2)}\n`;
  if (outPath === "-") {
    process.stdout.write(json);
    return;
  }
  const temporary = `${outPath}.tmp-${process.pid}`;
  writeFileSync(temporary, json);
  renameSync(temporary, outPath);
}

function finishReportClock(report: Report, startedClock: number): void {
  report.completed_at = new Date().toISOString();
  report.elapsed_seconds = round((performance.now() - startedClock) / 1_000);
}

function machineFacts() {
  const cpuList = cpus();
  return {
    platform: platform(),
    release: release(),
    arch: arch(),
    cpu_model: cpuList[0]?.model ?? "unknown",
    logical_cpu_count: cpuList.length,
    total_memory_bytes: totalmem(),
    bun_version: Bun.version,
    time_zone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    utc_offset_minutes: -new Date().getTimezoneOffset(),
  };
}

function formatMiB(kib: number): string {
  return `${(kib / 1_024).toFixed(1)}MiB`;
}

function round(value: number): number {
  return Math.round(value * 1_000_000) / 1_000_000;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function installSignalHandlers(): void {
  if (signalHandlersInstalled) return;
  signalHandlersInstalled = true;
  const handle = (signal: "SIGHUP" | "SIGINT" | "SIGTERM", exitCode: number) => {
    if (signalShutdown) return;
    signalShutdown = (async () => {
      console.error(`bench-persona: received ${signal}; cleaning up`);
      if (activeRun) {
        await terminateProcessTree(activeRun.child, activeRun.target);
        activeRun = null;
      }
      try {
        persistInterruptedReport?.(signal);
      } catch (error) {
        console.error(
          `bench-persona: unable to persist interrupted report: ${errorMessage(error)}`,
        );
      }
      process.exit(exitCode);
    })();
  };
  process.on("SIGHUP", () => handle("SIGHUP", 129));
  process.on("SIGINT", () => handle("SIGINT", 130));
  process.on("SIGTERM", () => handle("SIGTERM", 143));
}

function usage(): string {
  return `usage: bun scripts/bench-persona.ts \\
  --reference-bin PATH --reference-root PATH \\
  --pocket-bin PATH --library PATH --bundle PATH \\
  [--max-fps FPS] [--max-texture-dim PIXELS] \\
  [--settle 15] [--samples 13] [--interval 5] [--out PATH]

Runs the Electron reference first, terminates its full process tree, then runs
the Pocket binary. Each target gets an isolated loopback bridge port and must
return {ok:true} from /health within ${READINESS_TIMEOUT_SECONDS}s. Only then
does it settle, capture a cumulative CPU-time baseline, and record resource plus
health samples after each --interval. Reference also requires a non-settings CDP
page, validated 430x680 DPR 1.5 WebGL canvas, and a whole-window rAF receipt;
Pocket health must report a configured, visible model and renderFps > 1. Use
--out - for JSON on stdout.`;
}
