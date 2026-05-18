// focus/tab-focus
//
// Open a decoy tab in the same cwd as the agent (forces tiebreak
// ambiguity), spawn the agent in a separate tab, then request focus
// through the control socket. Both assertions are required:
//   1. `focus-dispatched` reports `result === "ok"`.
//   2. The terminal-side `focusedTargetId()` readback equals the
//      agent's tab id — strategy success alone is a false-green per
//      the adversarial review.
//
// Skipped via `applies` on terminals that don't expose tty or pid
// (Ghostty 1.3.1 falls here): the corral's cwd-only strategy can't
// disambiguate same-cwd tabs, so the readback would fail by design.
// The scenario lands intact for future Ghostty versions and other
// terminals that do expose tty/pid.

import { randomUUID } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { Scenario, ScenarioCell, ScenarioContext } from "../harness/scenario.ts";
import { waitForDiscoveredByPid } from "../harness/wait.ts";

export const focusTabFocus: Scenario = {
  name: "focus/tab-focus",
  varies: "agent+terminal",
  applies: (cell: ScenarioCell) => {
    if (!cell.agent || !cell.terminal) return false;
    if (cell.agent.capabilities.runsWithoutTerminal) return false;
    if (!cell.terminal.capabilities.canReportFocusedTargetId) return false;
    // Corral needs tty OR pid to disambiguate two terminals at the
    // same cwd. Without either, focus_by_cwd picks an arbitrary
    // same-cwd terminal and the readback assertion will fail.
    return cell.terminal.capabilities.exposesTty || cell.terminal.capabilities.exposesPid;
  },
  async run(ctx: ScenarioContext): Promise<void> {
    if (!ctx.agent || !ctx.terminal) {
      throw new Error("scenario expects an agent + terminal cell");
    }
    const runDir = mkdtempSync(join(tmpdir(), "tab-focus-"));
    try {
      // Open the decoy tab first so it's *not* the most-recently
      // created terminal when the corral resolves focus.
      const decoy = await ctx.terminal.openTab({
        cwd: runDir,
        command: "sleep 600",
      });
      ctx.cleanup.registerTab(decoy.close);

      const sessionId = randomUUID();
      const proc = await ctx.agent.spawn({
        cwd: runDir,
        sessionId,
        terminal: ctx.terminal,
      });
      ctx.cleanup.registerProcess(proc.pid, proc.kill);
      if (proc.tab) ctx.cleanup.registerTab(proc.tab.close);

      const agentTabId = proc.tab?.id;
      if (!agentTabId) {
        throw new Error("agent.spawn returned a process without a tab");
      }
      if (agentTabId === decoy.id) {
        throw new Error("decoy and agent share a tab id — terminal binding bug");
      }

      // Wait for the corral to know about the agent so focus can
      // resolve it.
      await waitForDiscoveredByPid(ctx, proc.pid, 0, 15_000);

      const requestId = Date.now() & 0x7fff_ffff;
      const from = ctx.trace.cursor();
      await ctx.control.focus(proc.pid, requestId);

      const dispatched = await ctx.trace.waitFor(
        (e) => e.kind === "focus-dispatched" && e.request_id === requestId,
        { from, timeoutMs: 5_000 },
      );
      if (dispatched.kind !== "focus-dispatched") throw new Error("narrow");
      if (dispatched.result !== "ok") {
        throw new Error(`focus-dispatched result=${dispatched.result}, expected ok`);
      }

      // Give Ghostty a tick to settle the focus change before reading
      // it back. AppleScript focus completes synchronously, but the
      // selected-tab redraw can lag by tens of ms.
      await sleep(150);
      const focused = await ctx.terminal.focusedTargetId();
      if (focused !== agentTabId) {
        throw new Error(
          `focused target ${focused} != agent tab ${agentTabId} (decoy was ${decoy.id})`,
        );
      }
    } finally {
      try { rmSync(runDir, { recursive: true, force: true }); } catch {}
    }
  },
};

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
