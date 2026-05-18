// Harness entry point: parse CLI args, build the corral binary,
// resolve agent/terminal bindings, expand the matrix, drive each
// scenario through one shared corral process (or one per scenario
// when --isolate is on or `requiresFreshCorral: true`).
//
// Step 4 acceptance: `bun run tests --grep nothing` boots the corral,
// runs the (empty) preflight, runs zero scenarios, shuts down cleanly.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { ClaudeCliAgent } from "./agents/claude-cli.ts";
import { CodexCliAgent } from "./agents/codex-cli.ts";
import { ScenarioCleanup } from "./harness/cleanup.ts";
import { buildPlan, type PlannedRun } from "./harness/matrix.ts";
import { preflight, type PreflightRequirement } from "./harness/preflight.ts";
import { quiesce } from "./harness/quiesce.ts";
import type { Scenario } from "./harness/scenario.ts";
import { Corral } from "./harness/corral.ts";
import { bindingClearRebind } from "./scenarios/binding-clear-rebind.ts";
import { discoverySpawnDetect } from "./scenarios/discovery-spawn-detect.ts";
import { focusTabFocus } from "./scenarios/focus-tab-focus.ts";
import { metadataCaptured } from "./scenarios/metadata-captured.ts";
import { stateAwaitingUser } from "./scenarios/state-awaiting-user.ts";
import { stateTurnEnded } from "./scenarios/state-turn-ended.ts";
import { GhosttyTerminal } from "./terminals/ghostty.ts";
import type { AgentBinding } from "./agents/types.ts";
import type { TerminalBinding } from "./terminals/types.ts";

interface CliArgs {
  grep?: string;
  isolate: boolean;
  shuffle: boolean;
  seed?: number;
}

function parseArgs(argv: string[]): CliArgs {
  const out: CliArgs = { isolate: false, shuffle: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--grep") {
      out.grep = argv[++i];
    } else if (a === "--isolate") {
      out.isolate = true;
    } else if (a === "--shuffle") {
      out.shuffle = true;
    } else if (a === "--seed") {
      out.seed = Number(argv[++i]);
    } else {
      throw new Error(`unknown flag: ${a}`);
    }
  }
  return out;
}

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CORRAL_BINARY = resolve(REPO_ROOT, "target/debug/corral");

async function ensureCorralBuilt(): Promise<void> {
  if (existsSync(CORRAL_BINARY)) return;
  console.error(`harness: building corral (target/debug/corral missing)`);
  const result = spawnSync("cargo", ["build", "-p", "corral-app"], {
    cwd: REPO_ROOT,
    stdio: "inherit",
  });
  if (result.status !== 0) {
    throw new Error("harness: cargo build -p corral-app failed");
  }
}

/// Scenarios discovered for the run. Empty in step 4; step 5 registers
/// the first three. Keeping the list explicit (no fs glob) makes it
/// obvious which scenarios are wired up.
const SCENARIOS: Scenario[] = [
  discoverySpawnDetect,
  bindingClearRebind,
  focusTabFocus,
  stateAwaitingUser,
  stateTurnEnded,
  metadataCaptured,
];

/// Build the preflight requirements list from the scenarios + cells we
/// are about to run. A scenario that doesn't open a terminal tab
/// doesn't need TCC; the empty-plan case returns an empty list.
function preflightRequirements(plan: PlannedRun[]): PreflightRequirement[] {
  const reqs: PreflightRequirement[] = [];
  const seen = new Set<string>();
  for (const item of plan) {
    if (!item.cell.terminal) continue;
    if (seen.has(item.cell.terminal.bundleId)) continue;
    seen.add(item.cell.terminal.bundleId);
    reqs.push({
      target: item.cell.terminal.bundleId,
      appName: appNameFor(item.cell.terminal.name),
    });
  }
  return reqs;
}

function appNameFor(name: string): string {
  switch (name) {
    case "ghostty":
      return "Ghostty";
    case "iterm2":
      return "iTerm";
    case "terminal-app":
      return "Terminal";
    default:
      return name;
  }
}

async function runOne(
  corral: Corral,
  plan: PlannedRun,
): Promise<{ name: string; ok: boolean; error?: unknown }> {
  const cleanup = new ScenarioCleanup();
  const label = labelFor(plan);
  try {
    // Quiesce before each scenario so the trace cursor starts at a
    // known DiscoveryPassCompleted boundary. Cheap when the registry
    // is already idle.
    await quiesce(corral.control, corral.trace);
    await plan.scenario.run({
      ...plan.cell,
      control: corral.control,
      trace: corral.trace,
      cleanup,
    });
    return { name: label, ok: true };
  } catch (error) {
    return { name: label, ok: false, error };
  } finally {
    try {
      await cleanup.finish(corral.control, corral.trace);
    } catch (e) {
      console.error(`cleanup failed after ${label}:`, e);
    }
  }
}

function labelFor(plan: PlannedRun): string {
  const parts = [plan.scenario.name];
  if (plan.cell.agent) parts.push(`agent=${plan.cell.agent.name}`);
  if (plan.cell.terminal) parts.push(`terminal=${plan.cell.terminal.name}`);
  return parts.join(" ");
}

async function main(): Promise<number> {
  const args = parseArgs(process.argv.slice(2));
  await ensureCorralBuilt();

  const agents: AgentBinding[] = [new ClaudeCliAgent(), new CodexCliAgent()];
  const terminals: TerminalBinding[] = [new GhosttyTerminal()];

  const plan = buildPlan(SCENARIOS, {
    agents,
    terminals,
    grep: args.grep,
    shuffleSeed: args.shuffle ? (args.seed ?? Date.now()) : undefined,
  });

  await preflight(preflightRequirements(plan));

  if (plan.length === 0) {
    console.error(`harness: 0 scenarios matched${args.grep ? ` --grep '${args.grep}'` : ""}`);
    // Still boot+shutdown the corral so the acceptance test exercises
    // the lifecycle even when no scenarios are queued.
    const corral = await Corral.spawn({ binaryPath: CORRAL_BINARY });
    await corral.shutdown();
    return 0;
  }

  const results: Array<{ name: string; ok: boolean; error?: unknown }> = [];
  let corral: Corral | null = null;
  for (const item of plan) {
    const freshNeeded = args.isolate || item.scenario.requiresFreshCorral;
    if (freshNeeded && corral) {
      await corral.shutdown();
      corral = null;
    }
    if (!corral) {
      corral = await Corral.spawn({ binaryPath: CORRAL_BINARY });
    }
    results.push(await runOne(corral, item));
    if (freshNeeded) {
      await corral.shutdown();
      corral = null;
    }
  }
  if (corral) await corral.shutdown();
  for (const t of terminals) {
    await t.dispose?.();
  }

  const failed = results.filter((r) => !r.ok);
  for (const r of results) {
    const tag = r.ok ? "PASS" : "FAIL";
    console.error(`[${tag}] ${r.name}`);
    if (!r.ok) console.error(r.error);
  }
  console.error(`harness: ${results.length - failed.length}/${results.length} passed`);
  return failed.length === 0 ? 0 : 1;
}

main().then((code) => process.exit(code), (err) => {
  console.error(err);
  process.exit(1);
});
