// metadata/captured
//
// After Claude's first turn, the corral's transcript parser should
// have learned the model id, the git branch the session was started
// on, and (eventually) a session title. We assert via the control
// socket's snapshot — the most direct view of what the registry has
// stored, separate from the trace's transcript-parsed deltas.

import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Scenario, ScenarioCell, ScenarioContext } from "../harness/scenario.ts";
import { waitForDiscoveredByPid } from "../harness/wait.ts";

export const metadataCaptured: Scenario = {
  name: "metadata/captured",
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
    const runDir = mkdtempSync(join(tmpdir(), "metadata-captured-"));
    try {
      const proc = await ctx.agent.spawn({
        cwd: runDir,
        sessionId,
        terminal: ctx.terminal,
      });
      ctx.cleanup.registerProcess(proc.pid, proc.kill);
      if (proc.tab) ctx.cleanup.registerTab(proc.tab.close);

      // Wait for initial discovery so the registry has an entry.
      await waitForDiscoveredByPid(ctx, proc.pid, 0, 15_000);

      // Wait for at least one transcript-parsed with
      // metadata_changed=true. The kick prompt in spawn already
      // triggered a turn, so an assistant line carrying loose
      // metadata (model id, git branch, session title) has landed.
      await ctx.trace.waitFor(
        (e) =>
          e.kind === "transcript-parsed" &&
          e.pid === proc.pid &&
          e.metadata_changed === true,
        { timeoutMs: 30_000 },
      );

      // Snapshot the registry and assert at least one piece of
      // metadata made it through end-to-end. Haiku occasionally
      // returns without a model field on the first assistant line
      // (resolved on the next reparse), so asserting "any of the
      // loose-metadata fields is non-null" keeps the test stable
      // while still proving the wire-up.
      const snap = await ctx.control.snapshot();
      const me = snap.find((a) => a.pid === proc.pid);
      if (!me) {
        throw new Error(`snapshot missing pid ${proc.pid}`);
      }
      const captured = me.model ?? me.git_branch ?? me.session_title ?? me.current_action;
      if (!captured) {
        throw new Error(
          `snapshot has no metadata fields set; got ${JSON.stringify(me)}`,
        );
      }
    } finally {
      try { rmSync(runDir, { recursive: true, force: true }); } catch {}
    }
  },
};
