#!/usr/bin/env bun

import { createHash } from "node:crypto";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  renameSync,
  rmSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { platform, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  allocateLoopbackPorts,
  runPersonaBenchmark,
  terminateProcessTree,
  type TargetName,
} from "./bench-persona";

type Mode = "reference" | "pocket" | "bench";
type BenchmarkProfile = "production" | "controlled";
type SpawnedProcess = ReturnType<typeof Bun.spawn>;

interface CliOptions {
  mode: Mode;
  profile: BenchmarkProfile;
  cycles: number;
  realAudio: boolean;
  settleSeconds: number;
  sampleCount: number;
  intervalSeconds: number;
  outPath: string | null;
}

interface Asset {
  name: string;
  url: string;
  path: string;
  bytes: number;
  sha256: string;
}

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const OUT = join(ROOT, "out");
const REFERENCE_ROOT = join(OUT, "persona-reference");
const REFERENCE_STATE = join(OUT, "persona-reference-state");
const REFERENCE_REPOSITORY = "https://github.com/xikhar/persona.git";
const REFERENCE_COMMIT = "4efec3ac729944d0b36137dd8847cc1b488e0bcb";
const FIXTURE_LIBRARY = join(ROOT, "fixtures", "persona", "library.json");
const REFERENCE_LIBRARY = join(
  REFERENCE_ROOT,
  "public",
  "assets",
  "library.json",
);
const REFERENCE_ELECTRON = join(
  REFERENCE_ROOT,
  "node_modules",
  "electron",
  "dist",
  "Electron.app",
  "Contents",
  "MacOS",
  "Electron",
);
const REFERENCE_NATIVE_HELPER = join(
  REFERENCE_ROOT,
  "native",
  "bin",
  "darwin",
  "persona-audio-listener",
);
const POCKET_BINARY = join(ROOT, "target", "release", "pocket-persona");
const POCKET_GUEST_DIR = join(ROOT, "dist", "pocket-persona");
const POCKET_GUEST = join(POCKET_GUEST_DIR, "guest.js");
const READINESS_TIMEOUT_MS = 60_000;
const REQUEST_TIMEOUT_MS = 2_000;

const ASSETS: Asset[] = [
  {
    name: "AvatarSample_A.vrm",
    url: "https://dist.ayaka.moe/vrm-models/VRoid-Hub/AvatarSample-A/AvatarSample_A.vrm",
    path: join(ROOT, "assets", "AvatarSample_A.vrm"),
    bytes: 26_781_812,
    sha256: "2a0ccd84880b03d7b65503d8b6287f7a97f3bb4fab70a5fd0a47b433c97827f5",
  },
  {
    name: "idle_loop.vrma",
    url: "https://raw.githubusercontent.com/moeru-ai/airi/main/packages/stage-ui-three/src/assets/vrm/animations/idle_loop.vrma",
    path: join(ROOT, "assets", "idle_loop.vrma"),
    bytes: 157_664,
    sha256: "ace95ba6dcc0bdf2ed1081c002332b4184441117c8d543b6f642b3d2c5cf99be",
  },
];

let activeChild: SpawnedProcess | null = null;
let activeTarget: TargetName = "pocket";
let signalShutdown: Promise<void> | null = null;
let signalExitCode: number | null = null;
let benchmarkOwnsSignals = false;
const temporaryDirectories = new Set<string>();

if (import.meta.main) {
  process.exitCode = await main(Bun.argv.slice(2));
}

async function main(argv: string[]): Promise<number> {
  let options: CliOptions;
  try {
    options = parseArgs(argv);
  } catch (error) {
    console.error(errorMessage(error));
    console.error("");
    console.error(usage());
    return 2;
  }

  if (platform() !== "darwin") {
    console.error(
      "persona acceptance currently requires macOS because it launches Electron.app directly",
    );
    return 2;
  }

  installSignalHandlers();

  try {
    await preflight(options);
    await ensureAssets();
    await ensureReferenceCheckout();
    stageReferenceLibrary();

    if (options.mode === "reference") {
      await ensureReferenceBuild(options.realAudio);
      await runVisibleReference(options);
      return 0;
    }

    await ensurePocketBuild();
    if (options.mode === "pocket") {
      await runVisiblePocket(options);
      return 0;
    }

    await ensureReferenceBuild(false);
    return runBenchmark(options);
  } catch (error) {
    if (signalExitCode != null) return signalExitCode;
    console.error(`persona acceptance failed: ${errorMessage(error)}`);
    return 1;
  } finally {
    if (!benchmarkOwnsSignals) {
      await cleanupActiveChild();
      cleanupTemporaryDirectories();
    }
  }
}

function parseArgs(argv: string[]): CliOptions {
  const args = argv.filter((argument) => argument !== "--");
  if (args.includes("--help") || args.includes("-h")) {
    console.log(usage());
    process.exit(0);
  }

  const mode = args.shift();
  if (mode !== "reference" && mode !== "pocket" && mode !== "bench") {
    throw new Error("first argument must be reference, pocket, or bench");
  }

  let profile: BenchmarkProfile = "production";
  let cycles = 0;
  let realAudio = false;
  let settleSeconds = 30;
  let sampleCount = 9;
  let intervalSeconds = 5;
  let outPath: string | null = null;

  const readValue = (name: string): string => {
    const value = args.shift();
    if (value == null || value.startsWith("--")) {
      throw new Error(`${name} requires a value`);
    }
    return value;
  };
  const readNumber = (
    name: string,
    minimum: number,
    integer = false,
  ): number => {
    const value = Number(readValue(name));
    if (
      !Number.isFinite(value) ||
      value < minimum ||
      (integer && !Number.isInteger(value))
    ) {
      throw new Error(
        `${name} must be ${integer ? "an integer" : "a number"} >= ${minimum}`,
      );
    }
    return value;
  };

  while (args.length > 0) {
    const flag = args.shift();
    switch (flag) {
      case "--profile": {
        const value = readValue(flag);
        if (value !== "production" && value !== "controlled") {
          throw new Error("--profile must be production or controlled");
        }
        profile = value;
        break;
      }
      case "--cycles":
        cycles = readNumber(flag, 0, true);
        break;
      case "--real-audio":
        realAudio = true;
        break;
      case "--settle":
        settleSeconds = readNumber(flag, 0);
        break;
      case "--samples":
        sampleCount = readNumber(flag, 1, true);
        break;
      case "--interval":
        intervalSeconds = readNumber(flag, Number.EPSILON);
        break;
      case "--out":
        outPath = resolve(readValue(flag));
        break;
      default:
        throw new Error(`unknown argument ${flag}`);
    }
  }

  if (mode !== "bench" && profile !== "production") {
    throw new Error("--profile is only valid in bench mode");
  }
  if (mode === "bench" && realAudio) {
    throw new Error("--real-audio is only valid for accept:persona");
  }

  return {
    mode,
    profile,
    cycles,
    realAudio,
    settleSeconds,
    sampleCount,
    intervalSeconds,
    outPath,
  };
}

async function preflight(options: CliOptions): Promise<void> {
  await requireCommand("git", ["--version"]);
  if (options.mode !== "pocket") {
    const nodeVersion = await commandOutput(["node", "--version"], ROOT, "reference");
    const major = Number(nodeVersion.trim().replace(/^v/, "").split(".")[0]);
    if (!Number.isInteger(major) || major < 24) {
      throw new Error(`Persona requires Node.js >=24; found ${nodeVersion.trim()}`);
    }
    await requireCommand("npm", ["--version"]);
  }
  if (options.mode !== "reference") {
    await requireCommand("cargo", ["--version"]);
  }
  if (options.realAudio) {
    await requireCommand("xcrun", ["--version"]);
  }
}

async function requireCommand(command: string, args: string[]): Promise<void> {
  try {
    await commandOutput([command, ...args], ROOT, "pocket");
  } catch (error) {
    throw new Error(`${command} is required: ${errorMessage(error)}`);
  }
}

async function ensureAssets(): Promise<void> {
  mkdirSync(join(ROOT, "assets"), { recursive: true });
  for (const asset of ASSETS) {
    if (assetMatches(asset.path, asset)) {
      console.log(`asset   ${asset.name} (${asset.sha256.slice(0, 12)}…)`);
      continue;
    }

    console.log(`fetch   ${asset.url}`);
    const response = await fetch(asset.url, {
      signal: AbortSignal.timeout(120_000),
    });
    if (!response.ok) {
      throw new Error(`${asset.url}: HTTP ${response.status}`);
    }

    const temporary = `${asset.path}.tmp-${process.pid}`;
    try {
      await Bun.write(temporary, await response.arrayBuffer());
      assertAsset(temporary, asset);
      renameSync(temporary, asset.path);
    } finally {
      rmSync(temporary, { force: true });
    }
    console.log(`asset   ${asset.name} (${asset.sha256.slice(0, 12)}…)`);
  }
}

function assetMatches(path: string, asset: Asset): boolean {
  return (
    existsSync(path) &&
    statSync(path).isFile() &&
    statSync(path).size === asset.bytes &&
    sha256File(path) === asset.sha256
  );
}

function assertAsset(path: string, asset: Asset): void {
  if (!existsSync(path) || !statSync(path).isFile()) {
    throw new Error(`${asset.name} download did not produce a file`);
  }
  const bytes = statSync(path).size;
  const sha256 = sha256File(path);
  if (bytes !== asset.bytes || sha256 !== asset.sha256) {
    throw new Error(
      `${asset.name} failed integrity validation: expected ${asset.bytes} bytes ` +
        `${asset.sha256}, got ${bytes} bytes ${sha256}`,
    );
  }
}

async function ensureReferenceCheckout(): Promise<void> {
  const gitDirectory = join(REFERENCE_ROOT, ".git");
  if (!existsSync(gitDirectory)) {
    if (
      existsSync(REFERENCE_ROOT) &&
      (!statSync(REFERENCE_ROOT).isDirectory() ||
        readdirSync(REFERENCE_ROOT).length > 0)
    ) {
      throw new Error(
        `${REFERENCE_ROOT} exists but is not a tool-owned Persona checkout`,
      );
    }
    mkdirSync(REFERENCE_ROOT, { recursive: true });
    await runCommand(["git", "init"], REFERENCE_ROOT, "reference");
    await runCommand(
      ["git", "remote", "add", "origin", REFERENCE_REPOSITORY],
      REFERENCE_ROOT,
      "reference",
    );
  }

  const remote = (
    await commandOutput(
      ["git", "remote", "get-url", "origin"],
      REFERENCE_ROOT,
      "reference",
    )
  ).trim();
  if (remote !== REFERENCE_REPOSITORY) {
    throw new Error(
      `refusing unexpected Persona reference remote ${JSON.stringify(remote)}`,
    );
  }

  const head = await optionalCommandOutput(
    ["git", "rev-parse", "HEAD"],
    REFERENCE_ROOT,
    "reference",
  );
  if (head.trim() !== REFERENCE_COMMIT) {
    const status = (
      await commandOutput(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        REFERENCE_ROOT,
        "reference",
      )
    ).trim();
    if (status.length > 0) {
      throw new Error(
        `Persona reference has tracked edits and cannot switch commits: ${status}`,
      );
    }
    await runCommand(
      ["git", "fetch", "--depth=1", "origin", REFERENCE_COMMIT],
      REFERENCE_ROOT,
      "reference",
    );
    await runCommand(
      ["git", "checkout", "--detach", "FETCH_HEAD"],
      REFERENCE_ROOT,
      "reference",
    );
  }

  const verifiedHead = (
    await commandOutput(
      ["git", "rev-parse", "HEAD"],
      REFERENCE_ROOT,
      "reference",
    )
  ).trim();
  if (verifiedHead !== REFERENCE_COMMIT) {
    throw new Error(`Persona reference resolved to unexpected commit ${verifiedHead}`);
  }
}

function stageReferenceLibrary(): void {
  const assetsRoot = join(REFERENCE_ROOT, "public", "assets");
  const modelRoot = join(assetsRoot, "models");
  const animationRoot = join(assetsRoot, "animations");
  mkdirSync(modelRoot, { recursive: true });
  mkdirSync(animationRoot, { recursive: true });

  ensureSymlink(ASSETS[0].path, join(modelRoot, "model.vrm"));
  for (const name of [
    "idle",
    "talk1",
    "talk2",
    "greeting",
    "happy",
    "finger-gun",
    "dance",
  ]) {
    ensureSymlink(ASSETS[1].path, join(animationRoot, `${name}.vrma`));
  }

  const fixture = readFileSync(FIXTURE_LIBRARY);
  if (
    !existsSync(REFERENCE_LIBRARY) ||
    !readFileSync(REFERENCE_LIBRARY).equals(fixture)
  ) {
    writeFileSync(REFERENCE_LIBRARY, fixture);
  }
}

function ensureSymlink(source: string, target: string): void {
  if (existsSync(target) || isDanglingSymlink(target)) {
    const current = lstatSync(target);
    if (
      current.isSymbolicLink() &&
      resolve(dirname(target), readlinkSync(target)) === source
    ) {
      return;
    }
    if (current.isDirectory()) {
      throw new Error(`refusing to replace directory ${target}`);
    }
    unlinkSync(target);
  }
  symlinkSync(source, target, "file");
}

function isDanglingSymlink(path: string): boolean {
  try {
    return lstatSync(path).isSymbolicLink();
  } catch {
    return false;
  }
}

async function ensureReferenceBuild(realAudio: boolean): Promise<void> {
  const packageLock = join(REFERENCE_ROOT, "package-lock.json");
  const installFingerprint = sha256File(packageLock);
  const installMarker = join(REFERENCE_STATE, "install.sha256");
  if (
    readText(installMarker).trim() !== installFingerprint ||
    !existsSync(REFERENCE_ELECTRON)
  ) {
    console.log("setup   Persona npm dependencies");
    await runCommand(["npm", "ci"], REFERENCE_ROOT, "reference");
    writeText(installMarker, `${installFingerprint}\n`);
  }

  const buildFingerprint = sha256Text(
    [
      REFERENCE_COMMIT,
      installFingerprint,
      sha256File(FIXTURE_LIBRARY),
      ...ASSETS.map((asset) => asset.sha256),
    ].join("\n"),
  );
  const buildMarker = join(REFERENCE_STATE, "renderer.sha256");
  const builtModel = join(
    REFERENCE_ROOT,
    "dist",
    "assets",
    "models",
    "model.vrm",
  );
  if (
    readText(buildMarker).trim() !== buildFingerprint ||
    !existsSync(join(REFERENCE_ROOT, "dist", "index.html")) ||
    !existsSync(builtModel)
  ) {
    console.log("build   Persona renderer");
    await runCommand(["npm", "run", "build"], REFERENCE_ROOT, "reference");
    writeText(buildMarker, `${buildFingerprint}\n`);
  }

  if (realAudio && !existsSync(REFERENCE_NATIVE_HELPER)) {
    console.log("build   Persona Core Audio listener");
    await runCommand(
      ["npm", "run", "native:build"],
      REFERENCE_ROOT,
      "reference",
    );
  }
}

async function ensurePocketBuild(): Promise<void> {
  console.log("build   Pocket Persona guest");
  mkdirSync(POCKET_GUEST_DIR, { recursive: true });
  const result = await Bun.build({
    entrypoints: [join(ROOT, "crates", "pocket-persona", "guest", "main.ts")],
    outdir: POCKET_GUEST_DIR,
    naming: "guest.js",
    target: "browser",
    format: "iife",
    minify: true,
    sourcemap: "external",
  });
  if (!result.success) {
    for (const log of result.logs) console.error(log);
    throw new Error("Pocket Persona guest build failed");
  }

  console.log("build   Pocket Persona release binary");
  await runCommand(
    ["cargo", "build", "--release", "-p", "pocket-persona"],
    ROOT,
    "pocket",
  );
  if (!existsSync(POCKET_BINARY)) {
    throw new Error(`Pocket Persona binary is missing after build: ${POCKET_BINARY}`);
  }
}

async function runVisibleReference(options: CliOptions): Promise<void> {
  const [port] = await allocateLoopbackPorts(1);
  const userData = mkdtempSync(join(tmpdir(), "pocket-character-persona-"));
  temporaryDirectories.add(userData);
  const env = {
    ...process.env,
    PERSONA_BRIDGE_PORT: String(port),
    ...(options.realAudio
      ? {}
      : {
          // Visual acceptance drives the shared event contract directly. Avoid
          // attaching to an unrelated Codex/ChatGPT process or prompting for
          // System Audio Recording permission unless explicitly requested.
          PERSONA_TARGET_PROCESS_PATTERN: "a^",
        }),
  };

  console.log(
    options.realAudio
      ? "audio   real Persona listener enabled; macOS may request System Audio Recording"
      : "audio   visual event driver enabled; pass --real-audio to test Core Audio",
  );
  await runVisibleTarget(
    "reference",
    [
      REFERENCE_ELECTRON,
      REFERENCE_ROOT,
      `--user-data-dir=${userData}`,
    ],
    REFERENCE_ROOT,
    env,
    port,
    options.cycles,
  );
}

async function runVisiblePocket(options: CliOptions): Promise<void> {
  const [port] = await allocateLoopbackPorts(1);
  await runVisibleTarget(
    "pocket",
    [
      POCKET_BINARY,
      "--library",
      REFERENCE_LIBRARY,
      "--bundle",
      POCKET_GUEST,
      "--bridge-port",
      String(port),
      "--fps",
      "60",
      "--max-texture-dim",
      "2048",
    ],
    ROOT,
    process.env,
    port,
    options.cycles,
  );
}

async function runVisibleTarget(
  target: TargetName,
  argv: string[],
  cwd: string,
  env: Record<string, string | undefined>,
  port: number,
  cycles: number,
): Promise<void> {
  console.log(`launch  ${target}: ${formatCommand(argv)}`);
  const child = Bun.spawn(argv, {
    cwd,
    env,
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
    detached: true,
  });
  activeChild = child;
  activeTarget = target;

  try {
    await waitForVisibleReady(target, child, port);
    printVisualChecklist(target, port, cycles);
    await driveVisualSequence(child, port, cycles);
  } finally {
    await cleanupActiveChild();
  }
}

async function waitForVisibleReady(
  target: TargetName,
  child: SpawnedProcess,
  port: number,
): Promise<void> {
  const deadline = performance.now() + READINESS_TIMEOUT_MS;
  let lastFailure = "bridge has not responded";
  while (performance.now() < deadline) {
    assertChildRunning(child, `${target} exited before visual readiness`);
    try {
      const response = await fetch(`http://127.0.0.1:${port}/health`, {
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });
      const body = (await response.json()) as Record<string, unknown>;
      if (!response.ok || body.ok !== true) {
        lastFailure = `health returned ${response.status} ${JSON.stringify(body)}`;
      } else if (target === "pocket") {
        const status = body.status;
        if (
          status != null &&
          typeof status === "object" &&
          (status as Record<string, unknown>).modelConfigured === true &&
          (status as Record<string, unknown>).windowVisible === true &&
          typeof (status as Record<string, unknown>).renderFps === "number" &&
          ((status as Record<string, unknown>).renderFps as number) > 1
        ) {
          return;
        }
        lastFailure = `Pocket health is not render-ready: ${JSON.stringify(body)}`;
      } else {
        // Persona publishes bridge health before its renderer. Allow the
        // configured avatar window to finish WebGL/model initialization.
        await sleepWhileRunning(child, 3_000, "Persona exited during renderer warmup");
        return;
      }
    } catch (error) {
      lastFailure = errorMessage(error);
    }
    await sleepWhileRunning(child, 250, `${target} exited during readiness`);
  }
  throw new Error(`${target} was not ready within 60s: ${lastFailure}`);
}

function printVisualChecklist(
  target: TargetName,
  port: number,
  cycles: number,
): void {
  console.log("");
  console.log(`ready   ${target} at http://127.0.0.1:${port}`);
  console.log("check   full character in a transparent, frameless, topmost 430x680 window");
  console.log("check   idle loop, autonomous blink, and spring motion");
  console.log("check   speaking body + pulsed lips, then action crossfade and return");
  console.log("input   scroll zoom · left-drag orbit · right-drag pan");
  console.log(
    cycles === 0
      ? "demo    repeats until Ctrl-C"
      : `demo    ${cycles} cycle${cycles === 1 ? "" : "s"}, then exits`,
  );
  console.log("");
}

async function driveVisualSequence(
  child: SpawnedProcess,
  port: number,
  cycles: number,
): Promise<void> {
  let completed = 0;
  while (cycles === 0 || completed < cycles) {
    console.log(`demo    cycle ${completed + 1}: idle`);
    await postEvent(port, {
      type: "state",
      state: voiceState("inactive", "idle"),
    });
    await sleepWhileRunning(child, 2_500, "target exited during idle demo");

    console.log(`demo    cycle ${completed + 1}: speaking + lip sync`);
    await postEvent(port, {
      type: "state",
      state: voiceState("active", "speaking"),
    });
    const levels = [0.04, 0.16, 0.34, 0.12, 0.52, 0.24, 0.08, 0];
    for (let repeat = 0; repeat < 4; repeat++) {
      for (const level of levels) {
        await postEvent(port, { type: "audio-level", level });
        await sleepWhileRunning(
          child,
          110,
          "target exited during lip-sync demo",
        );
      }
    }

    console.log(`demo    cycle ${completed + 1}: listening`);
    await postEvent(port, {
      type: "state",
      state: voiceState("active", "listening"),
    });
    await sleepWhileRunning(child, 1_000, "target exited during listening demo");

    console.log(`demo    cycle ${completed + 1}: greeting action`);
    await postEvent(port, {
      type: "animation",
      animation_name: "greeting",
    });
    await sleepWhileRunning(child, 3_500, "target exited during action demo");
    completed++;
  }
}

function voiceState(
  phase: "inactive" | "active",
  activity: "idle" | "listening" | "speaking",
) {
  return {
    phase,
    activity,
    microphoneMuted: false,
    outputMuted: false,
  };
}

async function postEvent(port: number, body: unknown): Promise<void> {
  let lastFailure = "event request did not run";
  for (let attempt = 1; attempt <= 5; attempt++) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/events`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });
      const text = await response.text();
      if (response.status === 202) {
        let parsed: unknown;
        try {
          parsed = JSON.parse(text);
        } catch {
          throw new Error(`event returned invalid JSON: ${text}`);
        }
        if (
          parsed == null ||
          typeof parsed !== "object" ||
          (parsed as Record<string, unknown>).accepted !== true
        ) {
          throw new Error(`event was not accepted: ${text}`);
        }
        return;
      }
      lastFailure = `HTTP ${response.status}: ${text}`;
      if (response.status !== 502 && response.status !== 503) {
        break;
      }
    } catch (error) {
      lastFailure = errorMessage(error);
    }
    if (attempt < 5) await Bun.sleep(75);
  }
  throw new Error(`event ${JSON.stringify(body)} failed: ${lastFailure}`);
}

async function runBenchmark(options: CliOptions): Promise<number> {
  const profile =
    options.profile === "controlled"
      ? { fps: 120, texture: 4096 }
      : { fps: 60, texture: 2048 };
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const outPath =
    options.outPath ??
    join(
      OUT,
      "bench",
      `persona-${options.profile}-${profile.fps}hz-${profile.texture}-${stamp}.json`,
    );
  console.log(
    `bench   ${options.profile}: ${profile.fps} Hz / ${profile.texture}px textures`,
  );
  console.log("bench   keep each topmost window visible and leave input untouched");

  benchmarkOwnsSignals = true;
  try {
    return await runPersonaBenchmark([
      "--reference-bin",
      REFERENCE_ELECTRON,
      "--reference-root",
      REFERENCE_ROOT,
      "--pocket-bin",
      POCKET_BINARY,
      "--library",
      REFERENCE_LIBRARY,
      "--bundle",
      POCKET_GUEST,
      "--max-fps",
      String(profile.fps),
      "--max-texture-dim",
      String(profile.texture),
      "--settle",
      String(options.settleSeconds),
      "--samples",
      String(options.sampleCount),
      "--interval",
      String(options.intervalSeconds),
      "--out",
      outPath,
    ]);
  } finally {
    benchmarkOwnsSignals = false;
  }
}

async function runCommand(
  argv: string[],
  cwd: string,
  target: TargetName,
): Promise<void> {
  console.log(`run     ${formatCommand(argv)}`);
  const child = Bun.spawn(argv, {
    cwd,
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
    detached: true,
  });
  activeChild = child;
  activeTarget = target;
  const exitCode = await child.exited;
  if (activeChild?.pid === child.pid) activeChild = null;
  if (exitCode !== 0) {
    throw new Error(`${argv[0]} exited with status ${exitCode}`);
  }
}

async function commandOutput(
  argv: string[],
  cwd: string,
  target: TargetName,
): Promise<string> {
  const child = Bun.spawn(argv, {
    cwd,
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
    detached: true,
  });
  activeChild = child;
  activeTarget = target;
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (activeChild?.pid === child.pid) activeChild = null;
  if (exitCode !== 0) {
    throw new Error(
      `${formatCommand(argv)} exited with status ${exitCode}: ${stderr.trim()}`,
    );
  }
  return stdout;
}

async function optionalCommandOutput(
  argv: string[],
  cwd: string,
  target: TargetName,
): Promise<string> {
  try {
    return await commandOutput(argv, cwd, target);
  } catch {
    return "";
  }
}

async function sleepWhileRunning(
  child: SpawnedProcess,
  milliseconds: number,
  context: string,
): Promise<void> {
  const result = await Promise.race([
    Bun.sleep(milliseconds).then(() => null),
    child.exited,
  ]);
  if (result !== null) {
    throw new Error(`${context} (exit ${result})`);
  }
  assertChildRunning(child, context);
}

function assertChildRunning(child: SpawnedProcess, context: string): void {
  try {
    process.kill(child.pid, 0);
  } catch {
    throw new Error(context);
  }
}

async function cleanupActiveChild(): Promise<void> {
  const child = activeChild;
  if (child == null) return;
  activeChild = null;
  await terminateProcessTree(child, activeTarget);
}

function installSignalHandlers(): void {
  const handle = (
    signal: "SIGHUP" | "SIGINT" | "SIGTERM",
    exitCode: number,
  ) => {
    if (benchmarkOwnsSignals || signalShutdown != null) return;
    signalExitCode = exitCode;
    signalShutdown = (async () => {
      console.error(`persona acceptance: received ${signal}; cleaning up`);
      await cleanupActiveChild();
      cleanupTemporaryDirectories();
      process.exit(exitCode);
    })();
  };
  process.on("SIGHUP", () => handle("SIGHUP", 129));
  process.on("SIGINT", () => handle("SIGINT", 130));
  process.on("SIGTERM", () => handle("SIGTERM", 143));
}

function cleanupTemporaryDirectories(): void {
  for (const directory of temporaryDirectories) {
    rmSync(directory, { recursive: true, force: true });
    temporaryDirectories.delete(directory);
  }
}

function readText(path: string): string {
  return existsSync(path) ? readFileSync(path, "utf8") : "";
}

function writeText(path: string, value: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, value);
}

function sha256File(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function sha256Text(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function formatCommand(argv: string[]): string {
  return argv
    .map((argument) =>
      /^[A-Za-z0-9_./:=+-]+$/.test(argument)
        ? argument
        : JSON.stringify(argument),
    )
    .join(" ");
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function usage(): string {
  return `Pocket Persona acceptance

Usage:
  bun run accept:persona [-- --cycles N] [--real-audio]
  bun run accept:pocket [-- --cycles N]
  bun run bench:persona [-- --settle 30 --samples 9 --interval 5]
  bun run bench:persona:controlled [-- --settle 30 --samples 9 --interval 5]

The two visual commands stage the same pinned Persona catalog and repeat the
same idle, speaking/lip-sync, listening, and greeting sequence. N=0 (default)
repeats until Ctrl-C. Benchmark modes run Persona and Pocket sequentially and
write a timestamped JSON report under out/bench/.`;
}
