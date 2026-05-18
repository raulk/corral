// state/turn-ended
//
// Spawn Claude (the spawn() helper sends a kick prompt and waits for
// the agent to land in the registry). The kick prompt has Claude
// reply once and stop, so the transcript parser should emit
// `lifecycle: "turn-ended"` from a subsequent transcript-parsed
// event. This is the path that turns the strip back to its "idle"
// colour after a green "turn in progress".

import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Scenario, ScenarioCell, ScenarioContext } from "../harness/scenario.ts";
import { waitForDiscoveredByPid } from "../harness/wait.ts";

export const stateTurnEnded: Scenario = {
  name: "state/turn-ended",
  varies: "agent",
  applies: (cell: ScenarioCell) =>
    cell.agent !== null &&
    cell.agent.name === "claude-cli" &&
    cell.terminal !== null,
  async run(ctx: ScenarioContext): Promise<void> {
    if (!ctx.agent || !ctx.terminal) {
      throw new Error("scenario expects an agent + terminal cell");
    }
    const sessionId = randomUUID();
    const runDir = mkdtempSync(join(tmpdir(), "turn-ended-"));
    try {
      const proc = await ctx.agent.spawn({
        cwd: runDir,
        sessionId,
        terminal: ctx.terminal,
      });
      ctx.cleanup.registerProcess(proc.pid, proc.kill);
      if (proc.tab) ctx.cleanup.registerTab(proc.tab.close);

      await waitForDiscoveredByPid(ctx, proc.pid, 0, 15_000);

      // The kick prompt completes quickly on haiku; the assistant
      // reply lands within ~10s and the parser emits turn-ended.
      await ctx.trace.waitFor(
        (e) =>
          e.kind === "transcript-parsed" &&
          e.pid === proc.pid &&
          e.lifecycle === "turn-ended",
        { timeoutMs: 30_000 },
      );
    } finally {
      try { rmSync(runDir, { recursive: true, force: true }); } catch {}
    }
  },
};
