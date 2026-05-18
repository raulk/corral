// ClaudeCliAgent — drives the `claude` CLI installed on PATH.
//
// `spawn` opens a terminal tab running `claude --session-id <uuid>`,
// then polls `pgrep -f` to find the matching Claude pid. The session
// id is generated per-call so two concurrent agents never share argv.
//
// `driveClear` types `/clear<enter>` plus a follow-up prompt through
// the terminal binding's paste-style `inputText` + `sendKey` API. We
// rely on the terminal to route the bytes straight to the pty so we
// never steal keyboard focus from the user.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { randomUUID } from "node:crypto";

import type {
  AgentBinding,
  AgentCapabilities,
  AgentProcess,
  SpawnArgs,
} from "./types.ts";
import type { TerminalBinding } from "../terminals/types.ts";

export interface ClaudeCliOptions {
  binaryPath?: string;
}

const SPAWN_TIMEOUT_MS = 10_000;

export class ClaudeCliAgent implements AgentBinding {
  readonly name = "claude-cli" as const;
  readonly binaryPath: string;
  readonly probedVersion: string | null;
  readonly capabilities: AgentCapabilities;

  constructor(opts: ClaudeCliOptions = {}) {
    this.binaryPath = opts.binaryPath ?? resolveOnPath("claude");
    this.probedVersion = probeVersion(this.binaryPath);
    this.capabilities = capabilitiesFor(this.probedVersion);
  }

  async spawn(args: SpawnArgs): Promise<AgentProcess> {
    if (!args.terminal) {
      throw new Error("ClaudeCliAgent.spawn requires a terminal binding");
    }
    const terminal = args.terminal;
    const sessionId = args.sessionId ?? randomUUID();
    // `--model haiku` keeps the per-spawn API cost minimal; the
    // harness spawns Claude many times across the matrix. Scenarios
    // that need reliable tool-calling override via SpawnArgs.model
    // — haiku ignores AskUserQuestion-style instructions too often.
    const model = args.model ?? "haiku";
    const command =
      `${shellQuote(this.binaryPath)} --session-id ${sessionId} --model ${model}`;
    const tab = await terminal.openTab({ cwd: args.cwd, command });
    const pid = await waitForPid(sessionId, SPAWN_TIMEOUT_MS);
    // Claude doesn't write its session record / transcript until it
    // completes an API turn, so the corral's discovery has nothing
    // to bind to until we prompt it. Wait for the TUI to draw, then
    // paste "hi" + Enter through the terminal binding (pty-level
    // injection, no focus theft).
    await sleep(800);
    await terminal.inputText(tab.id, "hi");
    await terminal.sendKey(tab.id, "enter");
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

  async driveClear(proc: AgentProcess): Promise<void> {
    if (!proc.tab || !proc.terminal) {
      throw new Error("driveClear needs an agent attached to a terminal");
    }
    // Slash-command first, then a follow-up prompt so the new session
    // actually writes its record + transcript (same reason as the
    // kick in `spawn`).
    await proc.terminal.inputText(proc.tab.id, "/clear");
    await proc.terminal.sendKey(proc.tab.id, "enter");
    await sleep(800);
    await proc.terminal.inputText(proc.tab.id, "hi");
    await proc.terminal.sendKey(proc.tab.id, "enter");
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
  const line = (out.stdout + out.stderr).split("\n").find((l) => /\d/.test(l));
  return line ? line.trim() : null;
}

function capabilitiesFor(version: string | null): AgentCapabilities {
  const semver = version ? matchSemver(version) : null;
  const writesSessionRecord = !semver || cmp(semver, [2, 0, 0]) >= 0;
  const recordHasStartedAt = !semver || cmp(semver, [2, 1, 140]) >= 0;
  return {
    driveableClear: true,
    driveableResume: true,
    writesSessionRecord,
    recordHasStartedAt,
    runsWithoutTerminal: false,
  };
}

function matchSemver(s: string): [number, number, number] | null {
  const m = s.match(/(\d+)\.(\d+)\.(\d+)/);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

function cmp(a: [number, number, number], b: [number, number, number]): number {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] - b[i];
  }
  return 0;
}

function shellQuote(s: string): string {
  // claude command runs as Ghostty's `command:` property, which is
  // passed to `/bin/sh -c`. Single-quote escape per POSIX.
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/// Find the pid of the Claude process whose argv contains the given
/// session id. Ghostty wraps each command in `/usr/bin/login -fl
/// <user> /bin/bash --noprofile --norc -c "exec -l <cmd>"`, so naive
/// `pgrep -f` matches the login wrapper *and* the inner claude. We
/// want the inner one (its `comm` is "claude"; the corral discovers
/// it by binary basename).
async function waitForPid(sessionId: string, timeoutMs: number): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  const needle = `--session-id ${sessionId}`;
  while (Date.now() < deadline) {
    const out = spawnSync("/bin/ps", ["-axww", "-o", "pid=,comm=,command="], {
      encoding: "utf-8",
    });
    if (out.status === 0) {
      for (const line of out.stdout.split("\n")) {
        if (!line.includes(needle)) continue;
        // Parse `pid comm command` — comm is truncated to ~16 chars
        // and on a login shell starts with `-`, so we ignore comm
        // entirely and instead inspect the argv0 in `command`.
        const trimmed = line.trim();
        const firstSp = trimmed.indexOf(" ");
        if (firstSp === -1) continue;
        const pid = Number(trimmed.slice(0, firstSp));
        if (!Number.isFinite(pid)) continue;
        // Skip past comm (next whitespace-delimited token) to reach command.
        const afterPid = trimmed.slice(firstSp + 1).trimStart();
        const commEnd = afterPid.indexOf(" ");
        if (commEnd === -1) continue;
        const command = afterPid.slice(commEnd + 1).trimStart();
        // argv0 is the first token of command; strip any leading `-`
        // (login shells) and check whether it ends with `/claude`.
        const argv0End = command.indexOf(" ");
        const argv0 = argv0End === -1 ? command : command.slice(0, argv0End);
        const exec = argv0.startsWith("-") ? argv0.slice(1) : argv0;
        if (exec.endsWith("/claude") || exec === "claude") return pid;
      }
    }
    await sleep(50);
  }
  throw new Error(
    `agent: claude with session-id ${sessionId} did not appear within ${timeoutMs}ms`,
  );
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
    // ESRCH — process already gone.
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
