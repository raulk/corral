// state/awaiting-user
//
// Drive Claude to invoke its built-in `AskUserQuestion` tool and
// assert the corral's transcript parser emits a `transcript-parsed`
// event with `lifecycle: "awaiting-user"`. This is the path that
// makes the strip turn pink ("the agent is blocked on a structured
// question") rather than green ("turn in progress").

import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Scenario, ScenarioCell, ScenarioContext } from "../harness/scenario.ts";
import { waitForDiscoveredByPid } from "../harness/wait.ts";

export const stateAwaitingUser: Scenario = {
  name: "state/awaiting-user",
  varies: "agent",
  applies: (cell: ScenarioCell) =>
    cell.agent !== null &&
    cell.agent.capabilities.driveableClear === true &&
    cell.terminal !== null,
  async run(ctx: ScenarioContext): Promise<void> {
    if (!ctx.agent || !ctx.terminal) {
      throw new Error("scenario expects an agent + terminal cell");
    }
    const sessionId = randomUUID();
    const runDir = mkdtempSync(join(tmpdir(), "awaiting-user-"));
    try {
      const proc = await ctx.agent.spawn({
        cwd: runDir,
        sessionId,
        terminal: ctx.terminal,
        // Sonnet reliably calls AskUserQuestion when instructed;
        // haiku ignores the instruction often enough to flake the
        // assertion. Cost: one short turn per scenario run.
        model: "sonnet",
      });
      ctx.cleanup.registerProcess(proc.pid, proc.kill);
      if (proc.tab) ctx.cleanup.registerTab(proc.tab.close);

      // Initial discovery + kick-prompt land via spawn(); wait until
      // the corral has the agent bound and the kick turn has ended
      // before we drive a second turn — otherwise our prompt either
      // queues behind the kick or races with the TUI's re-render.
      await waitForDiscoveredByPid(ctx, proc.pid, 0, 15_000);
      await ctx.trace.waitFor(
        (e) =>
          e.kind === "transcript-parsed" &&
          e.pid === proc.pid &&
          e.lifecycle === "turn-ended",
        { timeoutMs: 30_000 },
      );

      const cursor = ctx.trace.cursor();
      // Directive prompt: state explicitly that we want the
      // structured tool, name it, and tell the model not to answer
      // in prose. Sonnet still treats this as a soft hint sometimes,
      // so the scenario tolerates a single round of "claude answered
      // in prose instead" by polling its retry.
      const prompt =
        "I am running an automated test of your AskUserQuestion tool. " +
        "You MUST invoke the AskUserQuestion tool right now with " +
        "question='Pick a colour.' and options=['red','blue']. " +
        "Do not output any prose, do not call any other tool, just " +
        "call AskUserQuestion immediately.";
      await ctx.terminal.inputText(proc.tab!.id, prompt);
      await ctx.terminal.sendKey(proc.tab!.id, "enter");

      await ctx.trace.waitFor(
        (e) =>
          e.kind === "transcript-parsed" &&
          e.pid === proc.pid &&
          e.lifecycle === "awaiting-user",
        { from: cursor, timeoutMs: 60_000 },
      );
    } finally {
      try { rmSync(runDir, { recursive: true, force: true }); } catch {}
    }
  },
};
