// binding/clear-rebind
//
// Spawn Claude with a known session id, wait for discovery, drive
// `/clear`, then assert exactly one `agent-rebound` event with a new
// transcript path followed by exactly one `agent-discovered` re-emit
// for the new binding. The scenario tests the registry's
// same-pid-different-transcript path (`5f4ec9e`-era regression).

import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Scenario, ScenarioCell, ScenarioContext } from "../harness/scenario.ts";
import { waitForDiscoveredByPid } from "../harness/wait.ts";

export const bindingClearRebind: Scenario = {
  name: "binding/clear-rebind",
  varies: "agent",
  applies: (cell: ScenarioCell) =>
    cell.agent !== null &&
    cell.agent.capabilities.driveableClear === true &&
    cell.terminal !== null,
  async run(ctx: ScenarioContext): Promise<void> {
    if (!ctx.agent || !ctx.terminal) {
      throw new Error("scenario expects an agent + terminal cell");
    }
    if (!ctx.agent.driveClear) {
      throw new Error("agent claims driveableClear but exposes no driveClear method");
    }
    const sessionId = randomUUID();
    const runDir = mkdtempSync(join(tmpdir(), "clear-rebind-"));
    try {
      const proc = await ctx.agent.spawn({
        cwd: runDir,
        sessionId,
        terminal: ctx.terminal,
      });
      ctx.cleanup.registerProcess(proc.pid, proc.kill);
      if (proc.tab) ctx.cleanup.registerTab(proc.tab.close);

      // Wait for the initial discovery — the registry needs to know
      // about the agent before /clear can rebind it.
      const discovered = await waitForDiscoveredByPid(ctx, proc.pid, 0, 15_000);
      const oldTranscript = discovered.transcript;

      // Drive /clear in the agent's terminal. Claude reacts by
      // closing the current transcript and starting a new one with a
      // fresh session id; the corral sees the new transcript on the
      // next discovery tick.
      const reboundCursor = ctx.trace.cursor();
      await ctx.agent.driveClear(proc);

      // Force a discovery so we don't wait the full 2s tick.
      await ctx.control.discoverNow();
      const rebound = await ctx.trace.waitFor(
        (e) =>
          e.kind === "agent-rebound" &&
          e.pid === proc.pid &&
          e.old_transcript === oldTranscript &&
          e.new_transcript !== oldTranscript,
        { from: reboundCursor, timeoutMs: 10_000 },
      );
      if (rebound.kind !== "agent-rebound") throw new Error("narrow");

      // The rebind path re-emits agent-discovered for the new binding.
      await ctx.trace.waitFor(
        (e) =>
          e.kind === "agent-discovered" &&
          e.pid === proc.pid &&
          e.transcript === rebound.new_transcript,
        { from: reboundCursor, timeoutMs: 5_000 },
      );
    } finally {
      try { rmSync(runDir, { recursive: true, force: true }); } catch {}
    }
  },
};
