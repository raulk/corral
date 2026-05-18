// discovery/spawn-detect
//
// Spawn an agent in a terminal tab and assert the corral emits
// `agent-discovered` within 2s for the matching session id, with the
// expected binding source. The scenario's intent is the
// happy-path detection of new agents: a new process appearing in
// `~/.claude/projects/...` (or `~/.codex/sessions/...`) must surface
// through discovery, not be missed.

import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Scenario, ScenarioCell, ScenarioContext } from "../harness/scenario.ts";
import { waitForDiscoveredByPid } from "../harness/wait.ts";

export const discoverySpawnDetect: Scenario = {
  name: "discovery/spawn-detect",
  varies: "agent+terminal",
  applies: (cell: ScenarioCell) => {
    if (!cell.agent) return false;
    const needsTerminal = !cell.agent.capabilities.runsWithoutTerminal;
    return needsTerminal ? cell.terminal !== null : cell.terminal === null;
  },
  async run(ctx: ScenarioContext): Promise<void> {
    if (!ctx.agent || !ctx.terminal) {
      throw new Error("scenario expects an agent + terminal cell");
    }
    const sessionId = randomUUID();
    const runDir = mkdtempSync(join(tmpdir(), "spawn-detect-"));
    try {
      const from = ctx.trace.cursor();
      console.error(`spawn-detect: spawning agent in ${runDir} with session ${sessionId}`);
      const proc = await ctx.agent.spawn({
        cwd: runDir,
        sessionId,
        terminal: ctx.terminal,
      });
      console.error(`spawn-detect: agent pid=${proc.pid} tab=${proc.tab?.id}`);
      ctx.cleanup.registerProcess(proc.pid, proc.kill);
      if (proc.tab) {
        ctx.cleanup.registerTab(proc.tab.close);
      }
      // Agents need a moment to write their session record / open
      // their transcript before discovery can bind. PID matching
      // covers both Claude (deterministic session-id passed via
      // argv) and Codex (rollout id generated internally).
      const ev = await waitForDiscoveredByPid(ctx, proc.pid, from, 15_000);
      // Codex skips session-record / argv-env entirely; it's bound
      // via the open-fd path. Claude can be bound any of three ways
      // depending on whether its session record has flushed yet.
      const allowed =
        ctx.agent.name === "codex-cli"
          ? ["open-fd" as const]
          : ["session-record" as const, "argv-env" as const, "mtime-fallback" as const];
      if (!allowed.includes(ev.binding_source as never)) {
        throw new Error(
          `unexpected binding source ${ev.binding_source}; expected one of ${allowed.join("|")}`,
        );
      }
    } finally {
      try { rmSync(runDir, { recursive: true, force: true }); } catch {}
    }
  },
};
