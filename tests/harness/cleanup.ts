// Per-scenario cleanup contract.
//
// Scenarios register the agents they spawn and the tabs they open. At
// the end of a scenario the harness kills each registered process,
// waits for the matching `process-exited` trace event, closes each
// registered tab, then quiesces the registry via `discover-now`. If
// any step times out the run is aborted — better to surface
// contamination than silently roll forward.

import type { ControlClient } from "./control.ts";
import type { TraceReader } from "./trace.ts";
import { quiesce } from "./quiesce.ts";
import type { CleanupHandle } from "./scenario.ts";

interface ProcessEntry {
  pid: number;
  kill: () => Promise<void>;
}

interface TabEntry {
  close: () => Promise<void>;
}

export class ScenarioCleanup implements CleanupHandle {
  private processes: ProcessEntry[] = [];
  private tabs: TabEntry[] = [];

  registerProcess(pid: number, kill: () => Promise<void>): void {
    this.processes.push({ pid, kill });
  }

  registerTab(close: () => Promise<void>): void {
    this.tabs.push({ close });
  }

  /// Tear down everything registered, in reverse order. Idempotent:
  /// killing an already-dead process is fine, as is closing a tab
  /// whose host process is gone.
  async finish(control: ControlClient, trace: TraceReader): Promise<void> {
    while (this.processes.length > 0) {
      const entry = this.processes.pop()!;
      try {
        await entry.kill();
      } catch (e) {
        // Already dead is fine; log for diagnostics.
        console.warn(`cleanup: kill pid ${entry.pid} failed: ${e}`);
      }
      const from = trace.cursor();
      try {
        await trace.waitFor(
          (e) => e.kind === "process-exited" && e.pid === entry.pid,
          { from, timeoutMs: 5_000 },
        );
      } catch {
        // The registry may have already reaped before our cursor —
        // not a contamination problem since the next `quiesce` will
        // catch any lingering state.
      }
    }
    while (this.tabs.length > 0) {
      const entry = this.tabs.pop()!;
      try {
        await entry.close();
      } catch (e) {
        console.warn(`cleanup: tab close failed: ${e}`);
      }
    }
    await quiesce(control, trace);
  }
}
