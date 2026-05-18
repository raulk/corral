// CodexCliAgent — drives the `codex` interactive CLI installed on PATH.
//
// Unlike Claude, Codex doesn't accept a deterministic session-id
// argument from the harness — it generates the rollout UUID itself —
// so we identify the freshly-spawned pid via a pgrep diff:
//   1. snapshot existing codex pids
//   2. open the Ghostty tab running `codex`
//   3. poll for a new pid that wasn't in the snapshot
// As long as the harness only spawns one codex at a time, this is
// unambiguous.
//
// Codex writes its rollout file (`~/.codex/sessions/YYYY/...`) at
// startup, so unlike Claude no "kick prompt" is needed for discovery
// to bind. The corral's `open-fd` strategy picks it up on the next
// discovery tick.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";

import type {
  AgentBinding,
  AgentCapabilities,
  AgentProcess,
  SpawnArgs,
} from "./types.ts";

export interface CodexCliOptions {
  binaryPath?: string;
}

const SPAWN_TIMEOUT_MS = 10_000;

export class CodexCliAgent implements AgentBinding {
  readonly name = "codex-cli" as const;
  readonly binaryPath: string;
  readonly probedVersion: string | null;
  readonly capabilities: AgentCapabilities;

  constructor(opts: CodexCliOptions = {}) {
    this.binaryPath = opts.binaryPath ?? resolveOnPath("codex");
    this.probedVersion = probeVersion(this.binaryPath);
    this.capabilities = {
      // `/clear` isn't exposed through the same TUI affordance Claude
      // has; leave this off until a dedicated driver lands.
      driveableClear: false,
      driveableResume: false,
      writesSessionRecord: true,
      recordHasStartedAt: true,
      runsWithoutTerminal: false,
    };
  }

  async spawn(args: SpawnArgs): Promise<AgentProcess> {
    if (!args.terminal) {
      throw new Error("CodexCliAgent.spawn requires a terminal binding");
    }
    const terminal = args.terminal;
    const before = listCodexPids();
    // `codex [PROMPT]` starts the interactive session pre-loaded
    // with a prompt — far more reliable than typing "hi" into the
    // TUI after spawn, where the input layer may or may not be
    // ready yet. Codex makes the API turn immediately and writes
    // its rollout file before the corral's first discovery tick.
    const command = `${shellQuote(this.binaryPath)} 'hi'`;
    const tab = await terminal.openTab({ cwd: args.cwd, command });
    const pid = await waitForNewPid(before, SPAWN_TIMEOUT_MS);
    return {
      pid,
      tab,
      terminal,
      kill: async () => {
        kill(pid);
        await tab.close();
      },
      exited: waitForExit(pid),
    };
  }
}

function resolveOnPath(name: string): string {
  const which = spawnSync("/usr/bin/which", [name], { encoding: "utf-8" });
  if (which.status === 0 && which.stdout.trim().length > 0) {
    const path = which.stdout.trim();
    if (existsSync(path)) return path;
  }
  throw new Error(`agent: \`${name}\` not found on PATH`);
}

function probeVersion(binary: string): string | null {
  const out = spawnSync(binary, ["--version"], { encoding: "utf-8" });
  if (out.status !== 0) return null;
  return out.stdout.trim() || null;
}

function shellQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/// Set of pids running the codex binary. Filters by `command`'s
/// first token (the kernel's exec path) so we never include
/// `/usr/bin/login` / `/bin/bash` wrappers Ghostty puts in front of
/// the command. The corral discovers by the same basename rule.
function listCodexPids(): Set<number> {
  const out = spawnSync("/bin/ps", ["-axww", "-o", "pid=,command="], {
    encoding: "utf-8",
  });
  const pids = new Set<number>();
  if (out.status !== 0) return pids;
  for (const line of out.stdout.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const firstSp = trimmed.indexOf(" ");
    if (firstSp === -1) continue;
    const pid = Number(trimmed.slice(0, firstSp));
    if (!Number.isFinite(pid)) continue;
    const cmd = trimmed.slice(firstSp + 1).trimStart();
    const argv0End = cmd.indexOf(" ");
    const argv0 = argv0End === -1 ? cmd : cmd.slice(0, argv0End);
    const exec = argv0.startsWith("-") ? argv0.slice(1) : argv0;
    const basename = exec.includes("/") ? exec.slice(exec.lastIndexOf("/") + 1) : exec;
    if (basename === "codex" || basename.startsWith("codex-")) {
      pids.add(pid);
    }
  }
  return pids;
}

async function waitForNewPid(before: Set<number>, timeoutMs: number): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const pid of listCodexPids()) {
      if (!before.has(pid)) return pid;
    }
    await sleep(50);
  }
  throw new Error(`agent: no new codex pid appeared within ${timeoutMs}ms`);
}

async function waitForExit(pid: number): Promise<number | null> {
  while (true) {
    const out = spawnSync("/bin/ps", ["-p", String(pid)], { encoding: "utf-8" });
    if (out.status !== 0) return null;
    await sleep(100);
  }
}

function kill(pid: number): void {
  try {
    process.kill(pid, "SIGTERM");
  } catch {
    // ESRCH — already gone.
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
